//! Model alias layer: Codex slugs are exposed as `claude-<slug>[:<effort>]` so Claude Code's
//! gateway model discovery (which keeps only ids containing "claude") lists them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::upstream::{Upstream, UpstreamModel};

const PREFIX: &str = "claude-";
const CACHE_TTL: Duration = Duration::from_secs(600);
/// The upstream /models endpoint is slow and highly variable under load
/// (measured 1s–9s on a quota-throttled account). This fetch runs only when
/// the catalog cache is cold or stale — never on the request hot path — so it
/// can afford to wait. A short timeout here silently empties the catalog
/// after every restart, which propagates to the client model picker.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct PublicModel {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub created_at: String,
}

/// A public model id resolved to an upstream slug plus optional reasoning effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub slug: String,
    pub effort: Option<String>,
}

/// Split a public id into `(slug, effort)` without consulting the catalogue.
/// `claude-gpt-5.5:high` → ("gpt-5.5", Some("high")); `gpt-5.5` → ("gpt-5.5", None).
pub fn parse_id(id: &str) -> Resolved {
    let bare = id.strip_prefix(PREFIX).unwrap_or(id);
    match bare.rsplit_once(':') {
        Some((slug, effort)) if !effort.is_empty() && !slug.is_empty() => Resolved {
            slug: slug.to_string(),
            effort: Some(effort.to_string()),
        },
        _ => Resolved {
            slug: bare.to_string(),
            effort: None,
        },
    }
}

pub fn public_id(slug: &str, effort: Option<&str>) -> String {
    match effort {
        Some(e) => format!("{PREFIX}{slug}:{e}"),
        None => format!("{PREFIX}{slug}"),
    }
}

/// Resolve against a known model list. Returns None when the slug is unknown or the effort
/// suffix is not one the model supports.
pub fn resolve_in(models: &[UpstreamModel], id: &str) -> Option<Resolved> {
    let parsed = parse_id(id);
    let model = models.iter().find(|m| m.slug == parsed.slug)?;
    if let Some(effort) = &parsed.effort {
        let supported = model
            .supported_reasoning_levels
            .iter()
            .any(|l| &l.effort == effort);
        if !supported {
            return None;
        }
    }
    Some(parsed)
}

/// Main models exposed in the selector — one entry per base model, curated by
/// what the backend itself describes as its primary lineup (and each verified
/// by real inference on this account). Auxiliary (codex-auto-review), reserve
/// (gpt-reserve) and legacy (gpt-5.5, "Legacy coding model") slugs remain
/// resolvable by direct id but are not advertised.
const MAIN_SLUGS: &[&str] = &[
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

fn is_main(slug: &str) -> bool {
    MAIN_SLUGS.contains(&slug)
}

/// Effort levels the backend has been observed to declare (autenticated dump
/// 2026-10-13). Exposed for the redacted diagnostic log: only these are
/// printable, so a hostile `output_config.effort` can never forge journal
/// entries.
pub fn is_known_effort(e: &str) -> bool {
    crate::translate::is_known_effort(e)
}

pub fn public_list(models: &[UpstreamModel], created_at: &str) -> Vec<PublicModel> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in models {
        // One entry per base model. Effort is a request parameter (native
        // Claude Code effort control / id suffix), never a separate model id.
        if !is_main(&m.slug) {
            continue;
        }
        seen.insert(m.slug.clone());
        let display = if m.display_name.is_empty() {
            m.slug.clone()
        } else {
            m.display_name.clone()
        };
        out.push(PublicModel {
            id: public_id(&m.slug, None),
            display_name: display,
            description: m.description.clone(),
            kind: "model",
            created_at: created_at.to_string(),
        });
    }
    // Fleet finding: a main slug missing from the upstream catalog used to
    // vanish from the picker in silence. Loud now — once per slug per process
    // (cycle-3 #15: per-request spam in degraded mode).
    use std::sync::OnceLock;
    static WARNED: OnceLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
        OnceLock::new();
    let warned = WARNED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    for slug in MAIN_SLUGS {
        if !seen.contains(*slug) && warned.lock().unwrap().insert(slug) {
            tracing::warn!(slug, "main model missing from upstream catalog");
        }
    }
    out
}

struct Cached {
    models: Vec<UpstreamModel>,
    fetched: Instant,
}

/// Negative cache (cycle-3 #12/#15): a failed/empty upstream fetch within this
/// window makes the cold path return immediately instead of refetching —
/// stops per-request 30s stalls and warn spam while degraded.
const FETCH_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

pub struct Catalog {
    upstream: Arc<Upstream>,
    // Arc-wrapped so the background stale-while-revalidate task can hold a
    // reference across the tokio::spawn boundary.
    cache: Arc<RwLock<Option<Cached>>>,
    /// Serialises upstream refreshes so a cold or expired cache does not fan
    /// out into one `/models` call per concurrent request.
    refresh: Arc<Mutex<()>>,
    /// Last upstream fetch failure; within the backoff window the cold path
    /// serves stale/empty immediately (no second 30s stall in a row).
    last_failure: Arc<RwLock<Option<Instant>>>,
    started_at: String,
}

impl Catalog {
    pub fn new(upstream: Arc<Upstream>) -> Self {
        Self {
            upstream,
            cache: Arc::new(RwLock::new(None)),
            refresh: Arc::new(Mutex::new(())),
            last_failure: Arc::new(RwLock::new(None)),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }

    /// Current model list. Stale-while-revalidate: a fresh cache serves
    /// directly; an expired cache with content serves immediately and refreshes
    /// in the background; only a cold cache blocks on the upstream fetch, so
    /// no inference request ever pays the catalog round-trip twice in a row.
    pub async fn models(&self) -> Vec<UpstreamModel> {
        if let Some(models) = self.fresh().await {
            return models;
        }
        // Stale content on hand: serve it, refresh out-of-band.
        let stale = self.stale().await;
        if !stale.is_empty() {
            self.revalidate_background();
            return stale;
        }
        let _guard = self.refresh.lock().await;
        // Another request may have refreshed while we waited for the guard.
        if let Some(models) = self.fresh().await {
            return models;
        }
        // Negative cache (cycle-3 #12): a fetch failed moments ago — do not
        // stall this request for another UPSTREAM_TIMEOUT; serve what we have.
        if let Some(t) = *self.last_failure.read().await {
            if t.elapsed() < FETCH_FAILURE_BACKOFF {
                return self.stale().await;
            }
        }
        match self.upstream.list_models(UPSTREAM_TIMEOUT).await {
            Ok(models) if !models.is_empty() => {
                *self.cache.write().await = Some(Cached {
                    models: models.clone(),
                    fetched: Instant::now(),
                });
                *self.last_failure.write().await = None;
                models
            }
            Ok(_) => {
                tracing::warn!("upstream returned an empty model list");
                *self.last_failure.write().await = Some(Instant::now());
                self.stale().await
            }
            Err(e) => {
                tracing::warn!(error = %e, "model list fetch failed; serving cached list");
                *self.last_failure.write().await = Some(Instant::now());
                self.stale().await
            }
        }
    }

    /// Out-of-band catalog refresh, single-flight across callers.
    fn revalidate_background(&self) {
        let upstream = Arc::clone(&self.upstream);
        let cache = Arc::clone(&self.cache);
        let refresh = Arc::clone(&self.refresh);
        tokio::spawn(async move {
            // Hold the refresh mutex inside the task so concurrent callers
            // don't fan out into parallel /models fetches.
            let _guard = refresh.lock().await;
            // Someone else may have refreshed while this task queued on the mutex.
            let already_fresh = cache
                .read()
                .await
                .as_ref()
                .map(|c| c.fetched.elapsed() < CACHE_TTL)
                .unwrap_or(false);
            if already_fresh {
                return;
            }
            // Negative-cache backoff also applies to background refreshes.
            match upstream.list_models(UPSTREAM_TIMEOUT).await {
                Ok(models) if !models.is_empty() => {
                    *cache.write().await = Some(Cached {
                        models,
                        fetched: Instant::now(),
                    });
                }
                _ => {}
            }
        });
    }

    async fn fresh(&self) -> Option<Vec<UpstreamModel>> {
        self.cache
            .read()
            .await
            .as_ref()
            .filter(|c| c.fetched.elapsed() < CACHE_TTL)
            .map(|c| c.models.clone())
    }

    async fn stale(&self) -> Vec<UpstreamModel> {
        self.cache
            .read()
            .await
            .as_ref()
            .map(|c| c.models.clone())
            .unwrap_or_default()
    }

    pub async fn public_models(&self) -> Vec<PublicModel> {
        public_list(&self.models().await, &self.started_at)
    }

    pub async fn resolve(&self, id: &str) -> Option<Resolved> {
        resolve_in(&self.models().await, id)
    }

    /// Upstream metadata for a resolved slug, if the catalog knows it.
    pub async fn model(&self, slug: &str) -> Option<UpstreamModel> {
        self.models().await.into_iter().find(|m| m.slug == slug)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::ReasoningLevel;

    fn fixture() -> Vec<UpstreamModel> {
        vec![
            UpstreamModel {
                slug: "gpt-5.5".into(),
                display_name: "GPT-5.5".into(),
                description: "General model".into(),
                default_reasoning_level: Some("medium".into()),
                supported_reasoning_levels: vec![
                    ReasoningLevel {
                        effort: "low".into(),
                        description: "fast".into(),
                    },
                    ReasoningLevel {
                        effort: "high".into(),
                        description: "".into(),
                    },
                ],
            },
            UpstreamModel {
                slug: "gpt-6-astra".into(),
                display_name: "GPT-6-Astra".into(),
                description: "Frontier intelligence".into(),
                default_reasoning_level: Some("medium".into()),
                supported_reasoning_levels: vec![
                    ReasoningLevel {
                        effort: "low".into(),
                        description: "".into(),
                    },
                    ReasoningLevel {
                        effort: "high".into(),
                        description: "".into(),
                    },
                ],
            },
        ]
    }

    #[test]
    fn parse_strips_prefix_and_effort() {
        assert_eq!(
            parse_id("claude-gpt-5.5:high"),
            Resolved {
                slug: "gpt-5.5".into(),
                effort: Some("high".into())
            }
        );
        assert_eq!(
            parse_id("gpt-5.5"),
            Resolved {
                slug: "gpt-5.5".into(),
                effort: None
            }
        );
        assert_eq!(parse_id("claude-gpt-5.5:").slug, "gpt-5.5:");
    }

    #[test]
    fn alias_round_trip() {
        // The catalog advertises ONE entry per MAIN base model — no effort
        // variants, no legacy/auxiliary slugs. Effort is a request parameter.
        let list = public_list(&fixture(), "2026-01-01T00:00:00Z");
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-gpt-6-astra"]);
        assert_eq!(list[0].display_name, "GPT-6-Astra");
        assert_eq!(list[0].description, "Frontier intelligence");
        assert!(resolve_in(&fixture(), "claude-gpt-6-astra").is_some());
    }

    #[test]
    fn catalog_has_no_effort_duplicates() {
        // One entry per base model: no `:<effort>` ids in the public list.
        let list = public_list(&fixture(), "2026-01-01T00:00:00Z");
        assert!(list.iter().all(|m| !m.id.contains(':')));
        let mut ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids, deduped, "no duplicated base model in catalog");
    }

    #[test]
    fn non_main_models_still_resolve() {
        // Legacy/reserve/auxiliary slugs are not advertised but remain
        // resolvable by direct id, with the effort suffix for power use.
        assert!(resolve_in(&fixture(), "claude-gpt-5.5").is_some());
        assert!(resolve_in(&fixture(), "claude-gpt-5.5:high").is_some());
        assert!(resolve_in(&fixture(), "claude-gpt-6-astra:high").is_some());
    }

    #[test]
    fn resolve_rejects_unknown() {
        assert!(resolve_in(&fixture(), "claude-gpt-5.5:xhigh").is_none());
        assert!(resolve_in(&fixture(), "claude-sonnet-4").is_none());
        assert_eq!(
            resolve_in(&fixture(), "gpt-5.5:low").and_then(|r| r.effort),
            Some("low".to_string())
        );
    }
}
