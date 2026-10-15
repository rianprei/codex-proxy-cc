//! Reads and refreshes the Codex CLI's ChatGPT credentials in `<codex-home>/auth.json`.
//!
//! Token values are never logged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_AFTER_DAYS: i64 = 8;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Tokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    account_id: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    tokens: Tokens,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// The pair of values needed to authenticate an upstream request.
#[derive(Clone)]
pub struct Credentials {
    pub access_token: String,
    pub account_id: String,
}

pub struct AuthStore {
    path: PathBuf,
    http: reqwest::Client,
    inner: Mutex<AuthFile>,
}

impl AuthStore {
    pub async fn load(codex_home: &Path) -> Result<Self> {
        let path = codex_home.join("auth.json");
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("{} not found: run `codex login` first", path.display())
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let file: AuthFile =
            serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
        if file.tokens.access_token.is_empty() || file.tokens.refresh_token.is_empty() {
            bail!(
                "{} has no ChatGPT tokens: run `codex login` first",
                path.display()
            );
        }
        if file.tokens.account_id.is_empty() {
            bail!(
                "{} has no account_id: run `codex login` again",
                path.display()
            );
        }
        // The credentials mutex is held across refresh calls, so a stalled
        // request to the auth server must not block the proxy indefinitely.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            // Refresh é assíncrono e raro: mantém poucas conexões quentes.
            .pool_idle_timeout(Duration::from_secs(30))
            .http2_prior_knowledge()
            .no_gzip()
            .no_deflate()
            .no_brotli()
            .build()
            .context("build auth http client")?;
        Ok(Self {
            path,
            http,
            inner: Mutex::new(file),
        })
    }

    /// Current credentials, refreshing first when `last_refresh` is older than 8 days.
    pub async fn credentials(&self) -> Result<Credentials> {
        let mut file = self.inner.lock().await;
        if is_stale(file.last_refresh.as_deref(), Utc::now()) {
            if let Err(e) = self.refresh_locked(&mut file).await {
                tracing::warn!(error = %e, "proactive token refresh failed; using existing token");
            }
        }
        Ok(credentials_of(&file))
    }

    /// Refresh after an upstream 401. `rejected` is the access token that was rejected; if
    /// another request already refreshed past it, the newer token is returned without a
    /// second refresh.
    pub async fn refresh_after_401(&self, rejected: &str) -> Result<Credentials> {
        let mut file = self.inner.lock().await;
        if file.tokens.access_token != rejected {
            return Ok(credentials_of(&file));
        }
        self.refresh_locked(&mut file).await?;
        Ok(credentials_of(&file))
    }

    async fn refresh_locked(&self, file: &mut AuthFile) -> Result<()> {
        let body = RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: &file.tokens.refresh_token,
        };
        let resp = self
            .http
            .post(REFRESH_URL)
            .json(&body)
            .send()
            .await
            .context("token refresh request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            bail!("token refresh failed with HTTP {status}: {snippet}");
        }
        let parsed: RefreshResponse = resp.json().await.context("parse refresh response")?;
        if let Some(t) = parsed.access_token {
            file.tokens.access_token = t;
        }
        if let Some(t) = parsed.refresh_token {
            file.tokens.refresh_token = t;
        }
        if let Some(t) = parsed.id_token {
            file.tokens.id_token = t;
        }
        file.last_refresh = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true));
        write_atomic(&self.path, file).await?;
        tracing::info!("refreshed ChatGPT access token");
        Ok(())
    }
}

fn credentials_of(file: &AuthFile) -> Credentials {
    Credentials {
        access_token: file.tokens.access_token.clone(),
        account_id: file.tokens.account_id.clone(),
    }
}

fn is_stale(last_refresh: Option<&str>, now: DateTime<Utc>) -> bool {
    match last_refresh.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        Some(t) => now - t.with_timezone(&Utc) > TimeDelta::days(REFRESH_AFTER_DAYS),
        // Unknown age: do not hammer the refresh endpoint; a 401 forces a refresh anyway.
        None => false,
    }
}

async fn write_atomic(path: &Path, file: &AuthFile) -> Result<()> {
    let data = serde_json::to_vec_pretty(file)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, &data)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staleness_uses_eight_day_window() {
        let now = DateTime::parse_from_rfc3339("2026-09-10T00:00:00Z")
            .expect("valid")
            .with_timezone(&Utc);
        assert!(!is_stale(Some("2026-09-05T00:00:00Z"), now));
        assert!(is_stale(Some("2026-08-01T00:00:00.123456Z"), now));
        assert!(!is_stale(None, now));
        assert!(!is_stale(Some("garbage"), now));
    }

    #[test]
    fn auth_file_round_trips_unknown_fields() {
        let raw = r#"{"OPENAI_API_KEY":null,"auth_mode":"chatgpt","last_refresh":"2026-09-01T00:00:00Z","tokens":{"access_token":"a","refresh_token":"r","id_token":"i","account_id":"acc","extra_token_field":1}}"#;
        let file: AuthFile = serde_json::from_str(raw).expect("parse");
        assert_eq!(file.tokens.account_id, "acc");
        let back: Value = serde_json::to_value(&file).expect("serialize");
        assert_eq!(back["auth_mode"], "chatgpt");
        assert_eq!(back["OPENAI_API_KEY"], Value::Null);
        assert_eq!(back["tokens"]["extra_token_field"], 1);
    }
}
