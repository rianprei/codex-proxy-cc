//! Anthropic-compatible proxy for Codex (ChatGPT subscription) models.
//!
//! Split out from the binary so integration tests can exercise the HTTP surface,
//! the streaming state machine and the Anthropic<->Responses translation against
//! a mock upstream. The binary is a thin CLI over this library.

pub mod anthropic;
pub mod auth;
pub mod models;
pub mod server;
pub mod translate;
pub mod upstream;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::auth::AuthStore;
use crate::models::Catalog;
use crate::server::AppState;
use crate::upstream::Upstream;

/// Build the proxy state for an arbitrary Codex upstream base and auth dir.
///
/// Used by both the CLI and the integration tests; tests point `upstream_base`
/// at a local mock so no real inference and no real credentials are required.
///
/// `chain` is the opt-in same-provider fallback chain (Codex slugs in
/// preference order, from `CLAUDE_CODEX_CHAIN`). An EMPTY list disables the
/// feature entirely — behavior is byte-identical to the no-chain build.
pub async fn build_state(
    upstream_base: &str,
    codex_home: &Path,
    auth_token: Option<String>,
    default_effort: &str,
    chain: Vec<String>,
) -> Result<AppState> {
    let auth = Arc::new(AuthStore::load(codex_home).await?);
    let upstream = Arc::new(Upstream::new(upstream_base.to_string(), auth)?);
    let catalog = Arc::new(Catalog::new(upstream.clone()));
    Ok(AppState {
        auth_token: auth_token.filter(|t| !t.is_empty()),
        upstream,
        catalog,
        default_effort: default_effort.to_string(),
        chain: Arc::new(chain),
        chain_cooldowns: Arc::new(Mutex::new(HashMap::new())),
    })
}
