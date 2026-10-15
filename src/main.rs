use claudecodex::{server, upstream};

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Anthropic Messages API proxy for Codex (ChatGPT login) models.
#[derive(Parser, Debug)]
#[command(name = "claudecodex", version)]
struct Cli {
    /// TCP port to listen on.
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Bearer token Claude Code must present (its ANTHROPIC_AUTH_TOKEN). Unset disables the check.
    #[arg(long, env = "CLAUDECODEX_AUTH_TOKEN")]
    auth_token: Option<String>,
    /// Codex home directory containing auth.json (default: ~/.codex).
    #[arg(long, env = "CODEX_HOME")]
    codex_home: Option<PathBuf>,
    /// Upstream Codex backend base URL.
    #[arg(long, env = "CLAUDECODEX_UPSTREAM", default_value = upstream::DEFAULT_BASE)]
    upstream_base: String,
    /// Reasoning effort used when the model id carries no `:<effort>` suffix.
    #[arg(long, default_value = "medium")]
    default_reasoning_effort: String,
    /// Verbose logging (same as RUST_LOG=claudecodex=debug).
    #[arg(short, long)]
    verbose: bool,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Split a `CLAUDE_CODEX_CHAIN` value into Codex slugs, preserving order,
/// trimming whitespace, dropping empty entries (so a trailing comma or a
/// whitespace-only value cannot create phantom candidates) and collapsing
/// duplicates (a repeated entry would be re-attempted inside one walk and
/// open a bogus cooldown for its twin).
fn parse_chain(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for slug in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !out.iter().any(|x| x.as_str() == slug) {
            out.push(slug.to_string());
        }
    }
    out
}

/// The opt-in same-provider fallback chain: comma-separated Codex slugs in
/// preference order. Parsed ONCE at startup; per-candidate validation (catalog
/// membership) happens lazily per request. Unset or empty = feature off,
/// behavior byte-identical to the no-chain build.
fn chain_from_env() -> Vec<String> {
    parse_chain(&std::env::var("CLAUDE_CODEX_CHAIN").unwrap_or_default())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default_filter = if cli.verbose {
        "claudecodex=debug,info"
    } else {
        "info"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let codex_home = cli.codex_home.unwrap_or_else(|| home_dir().join(".codex"));
    let chain = chain_from_env();
    // Loud by design: a configured chain changes what serves a request, so
    // the operator must see it in the journal at startup.
    if !chain.is_empty() {
        tracing::info!(
            chain = %chain.join(","),
            "same-provider fallback chain enabled (opt-in)"
        );
    }
    let state = claudecodex::build_state(
        &cli.upstream_base,
        &codex_home,
        cli.auth_token.clone(),
        &cli.default_reasoning_effort,
        chain,
    )
    .await?;

    // Fleet finding: nothing warmed the catalog on boot, so the first
    // inference after a restart blocked on the cold upstream /models fetch
    // (up to 30s). Warm it out-of-band before accepting traffic.
    {
        let catalog = state.catalog.clone();
        tokio::spawn(async move {
            catalog.models().await;
        });
    }
    if state.auth_token.is_none() {
        tracing::warn!(
            "auth disabled: CLAUDECODEX_AUTH_TOKEN is unset or empty; any local process can use this proxy"
        );
    }
    let addr = format!("{}:{}", cli.bind, cli.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(
        %addr,
        auth_required = state.auth_token.is_some(),
        codex_home = %codex_home.display(),
        "claudecodex listening"
    );
    axum::serve(listener, server::router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_chain;

    #[test]
    fn chain_env_empty_or_blank_yields_no_chain() {
        assert!(parse_chain("").is_empty());
        assert!(parse_chain("   ").is_empty());
        assert!(parse_chain(",,,").is_empty());
    }

    #[test]
    fn chain_env_parses_in_order_with_trimming() {
        assert_eq!(
            parse_chain(" gpt-6-astra , gpt-5.6-sol ,,gpt-5.6-terra\t"),
            vec!["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra"]
        );
        // A trailing comma is a config typo, not a fourth candidate.
        assert_eq!(parse_chain("a,b,"), vec!["a", "b"]);
        // Duplicates collapse to the first occurrence, order preserved.
        assert_eq!(parse_chain("a, b ,a,a"), vec!["a", "b"]);
    }
}
