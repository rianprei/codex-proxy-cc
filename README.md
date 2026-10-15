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
ANTHROPIC_DEFAULT_OPUS_MODEL=claude-gpt-5.5 \
ANTHROPIC_DEFAULT_SONNET_MODEL=claude-gpt-5.5 \
ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-gpt-5.4-mini:low \
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
`:<effort>` suffix pins the reasoning effort, e.g. `claude-gpt-5.5:low`,
`claude-gpt-6-astra:xhigh`. Efforts are clamped to what the model supports.

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