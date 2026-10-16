# claudecodex

An Anthropic Messages-API-compatible HTTP proxy, written in Rust, that lets
[Claude Code](https://docs.anthropic.com/en/docs/claude-code) drive OpenAI /
Codex models through your existing ChatGPT (Codex) subscription.

Claude Code talks Anthropic `/v1/messages`; the proxy translates each request
into an OpenAI Responses API call against the Codex backend
(`chatgpt.com/backend-api/codex`), using the OAuth tokens the `codex` CLI
already stores in `~/.codex/auth.json`, and streams the answer back as
Anthropic SSE. No Codex app-server, no OpenAI API key.

## Prerequisites

- Rust toolchain (`cargo`).
- The Codex CLI, logged in with a ChatGPT account: `codex login`. The proxy
  reads (and, when the access token expires, refreshes and rewrites) only
  `auth.json` under the Codex home directory.

## Run

```sh
cargo run --release -- --auth-token secret
```

Then, in another terminal:

```sh
ANTHROPIC_BASE_URL="http://127.0.0.1:8787" \
ANTHROPIC_AUTH_TOKEN="secret" \
ANTHROPIC_API_KEY="" \
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
ANTHROPIC_DEFAULT_OPUS_MODEL=claude-gpt-6-astra \
ANTHROPIC_DEFAULT_SONNET_MODEL=claude-gpt-6-astra \
ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-gpt-5.6-luna:low \
CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING=1 \
claude
```

Claude Code fetches `GET /v1/models` | Anthropic-shaped list of `claude-<slug>` ids — one entry
per MAIN base model (no `:effort` variants; effort is a request parameter)
caches the list in `~/.claude/cache/gateway-models.json`, and lets you pick
any of the proxied models with `/model`.

## Model ids

Claude Code only keeps gateway models whose id contains `claude`, so every
Codex model is exposed as `claude-<codex slug>`; the proxy strips the prefix
before calling upstream (bare slugs are accepted too). An optional
`:<effort>` suffix pins the reasoning effort, e.g. `claude-gpt-6-astra:low`,
`claude-gpt-6-astra:xhigh`. Efforts are clamped to what the model supports.

The advertised lineup is one entry per base model, curated from what the
backend itself describes as its primary lineup (each verified by real
inference):

| Public id | Backend slug |
| --- | --- |
| `claude-gpt-6-astra` | `gpt-6-astra` (frontier) |
| `claude-gpt-6-sol` | `gpt-6-sol` |
| `claude-gpt-6-luna` | `gpt-6-luna` |
| `claude-gpt-6.1-sol` | `gpt-6.1-sol` |
| `claude-gpt-5.6-sol` | `gpt-5.6-sol` (workhorse) |
| `claude-gpt-5.6-terra` | `gpt-5.6-terra` (balanced) |
| `claude-gpt-5.6-luna` | `gpt-5.6-luna` (fast) |

Legacy (`gpt-5.5`), reserve (`gpt-reserve`) and auxiliary
(`codex-auto-review`) slugs stay resolvable by direct id but are not
advertised; a main slug the backend momentarily does not list is still
advertised in the picker (inference waits for the backend to expose it).

Effort precedence: the request's `output_config.effort` (the native Claude Code
effort control), then the `:<effort>` id suffix, then the request's
`thinking.budget_tokens`, then `--default-reasoning-effort`, clamped to what
the model advertises.

## Opt-in fallback chain (same provider only)

`CLAUDE_CODEX_CHAIN` is a comma-separated list of Codex slugs in preference
order:

```sh
CLAUDE_CODEX_CHAIN="gpt-6-astra,gpt-5.6-sol,gpt-5.6-terra" \
  cargo run --release -- --auth-token secret
```

When the selected model fails with a transient upstream error (429/5xx), the
proxy retries the request with the next chain entry after it — same Codex
backend, same ChatGPT account, never a paid API or another provider.

- **Quota exhaustion never chains.** "usage limit" is an account-level terminal
  state: every model would fail identically, so the terminal error surfaces
  immediately with `retry-after: 3600`, on any attempt.
- **Circuit breaker.** A candidate that failed with 429/5xx is skipped for
  60s (per-slug cooldown). The selected model is never keyed — it is always
  attempted first.
- **Loud, never silent.** Every switch logs a `warn` naming the original
  slug, the failing status, the new slug and the remaining candidates;
  exhaustion is logged too, and the per-attempt "model resolution" line shows
  the slug actually used.
- **Effort is re-resolved per candidate**, from the request-level channels
  (`output_config.effort`, `thinking`) or the CLI default, clamped to what
  the candidate declares — the selected model's `:effort` suffix does not
  carry over.
- **Off by default.** Unset or empty = the feature is fully disabled and
  behavior is identical to a no-chain build. A chain entry that does not
  resolve in the catalog is skipped with a warning.
- **Lenient parsing, strict behavior.** Entries are trimmed; empty and
  duplicate entries are dropped at startup (first occurrence wins).

## Build & footprint

The release profile (`opt-level = 3`, LTO, `codegen-units = 1`, `panic =
"abort"`, `strip`) plus a deliberately minimal dependency tree keep the
binary small: 8.8 MB naive → 3.7 MB release → **1.4 MB after UPX**.

- **10 direct dependencies** (`axum`, `bytes`, `futures-core`, `log`,
  `reqwest`, `serde`, `serde_json`, `thiserror`, `tokio`, `tokio-stream`):
  `anyhow`, `chrono`, `clap`, `tracing(+subscriber)`, `uuid`, `env_logger`
  and `futures-util` were replaced by ~175 lines of std-only code (util.rs)
  and a ~35-line stderr logger.
- **`axum` with `default-features = false`** (`http1`, `json`, `tokio` only):
  drops the transitive `tracing` tree axum 0.7 pulls in by default.
- **`futures-core` instead of `futures-util`**: only the `Stream` trait;
  `.next()` comes from the already-present `tokio_stream`.
- Granular tokio features (`rt`, `macros`, `net`, `time`, `sync`, `fs`)
  instead of `full`; reqwest without its default feature set (rustls, no
  gzip/deflate/brotli).

```sh
cargo build --release
upx --best --lzma target/release/claudecodex   # optional
```

## Layout

```
src/            proxy source (lib + bin split)
tests/          hermetic integration suite (mock upstream) + regression.sh
deploy/         launcher, multi-backend router, picker generator, systemd unit
docs/           architecture, test plan, research notes
```
