use claudecodex::{server, upstream, BoxResult};

use std::path::PathBuf;

/// CLI arguments parsed manually (no clap): 7 flags, all optional.
struct Cli {
    port: u16,
    bind: String,
    auth_token: Option<String>,
    codex_home: Option<PathBuf>,
    upstream_base: String,
    default_reasoning_effort: String,
    verbose: bool,
}

impl Cli {
    fn parse() -> Self {
        let mut cli = Self {
            port: 8787,
            bind: "127.0.0.1".to_string(),
            auth_token: std::env::var("CLAUDECODEX_AUTH_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            codex_home: std::env::var("CODEX_HOME").ok().map(PathBuf::from),
            upstream_base: upstream::DEFAULT_BASE.to_string(),
            default_reasoning_effort: "medium".to_string(),
            verbose: false,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            let next = |i: &mut usize| -> Option<String> {
                *i += 1;
                args.get(*i).cloned()
            };
            match arg.as_str() {
                "--port" => {
                    if let Some(v) = next(&mut i) {
                        cli.port = v.parse().unwrap_or(8787);
                    }
                }
                "--bind" => {
                    if let Some(v) = next(&mut i) {
                        cli.bind = v;
                    }
                }
                "--auth-token" => {
                    if let Some(v) = next(&mut i) {
                        cli.auth_token = Some(v);
                    }
                }
                "--codex-home" => {
                    if let Some(v) = next(&mut i) {
                        cli.codex_home = Some(PathBuf::from(v));
                    }
                }
                "--upstream-base" => {
                    if let Some(v) = next(&mut i) {
                        cli.upstream_base = v;
                    }
                }
                "--default-reasoning-effort" => {
                    if let Some(v) = next(&mut i) {
                        cli.default_reasoning_effort = v;
                    }
                }
                "-v" | "--verbose" => cli.verbose = true,
                _ => {} // ignore unknown args (user may pass --model etc.)
            }
            i += 1;
        }
        cli
    }
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> BoxResult<()> {
    let cli = Cli::parse();
    let default_filter = if cli.verbose {
        "claudecodex=debug,info"
    } else {
        "info"
    };
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(default_filter),
    )
    .init();

    let codex_home = cli.codex_home.unwrap_or_else(|| home_dir().join(".codex"));
    let chain = chain_from_env();
    // Loud by design: a configured chain changes what serves a request, so
    // the operator must see it in the journal at startup.
    if !chain.is_empty() {
        log::info!(
            "same-provider fallback chain enabled (opt-in): {}",
            chain.join(",")
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
        log::warn!(
            "auth disabled: CLAUDECODEX_AUTH_TOKEN is unset or empty; any local process can use this proxy"
        );
    }
    let addr = format!("{}:{}", cli.bind, cli.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    log::info!(
        "claudecodex listening addr={} auth_required={} codex_home={}",
        addr,
        state.auth_token.is_some(),
        codex_home.display()
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
