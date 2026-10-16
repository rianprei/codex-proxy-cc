# Changelog

All notable changes to this proxy, from the claude-nim mechanism port
(5b5215d) to the current HEAD (2ab6e2c). Earlier history (fork base,
fleet/cycle review fixes, LICENSE/NOTICE and the deployment layer) predates
this log; see NOTICE for provenance.

## 2ab6e2c — axum without default features

- `axum = { default-features = false, features = ["http1", "json", "tokio"] }`:
  axum 0.7's default set (`tracing`, `tower-log`, `matched-path`,
  `original-uri`, `query`, `form`) pulled the whole `tracing` crate back in as
  a transitive dependency. Only the features actually used remain; the
  residual `tracing` in the tree comes from `h2` (HTTP/2 mandatory) and LTO
  eliminates the dead code.
- Binary: 8.8 MB naive → 3.7 MB release → **1.4 MB UPX (−84%)**.
- Gates: 62/62 tests, clippy clean, fail-closed intact.

## 4028f74 — env_logger and futures-util removed

- `env_logger` → a ~35-line `StderrLogger`: reads `RUST_LOG` (loosest level
  wins), writes `[LEVEL] message` to stderr. Removes env_logger plus its
  anstream/anstyle/is-terminal formatting tree — journald adds timestamps.
- `futures-util` → `futures-core` (just the `Stream` trait) +
  `tokio_stream::StreamExt` (already present) for `.next()`.
- Direct dependencies: 17 → **10**. Binary: 4.2 MB release → 1.5 MB UPX.

## aeb368f — anyhow/chrono/clap/tracing/uuid removed

Extreme debloat: six dependencies replaced by ~140 lines of std-only code in
the new `util.rs`:

- `uuid` → `util::unique_id()` (nanosecond timestamp + AtomicU64).
- `chrono` → `util::rfc3339_now()`/`parse_rfc3339()` (Howard Hinnant's
  civil-date algorithm, std only).
- `anyhow` → `BoxResult<T>` + `UpstreamError::Msg(String)`.
- `clap` → manual CLI parsing (7 flags).
- `tracing`/`tracing-subscriber` → `log` + `env_logger` (removed entirely in
  the next commit).

Binary: 5.1 MB release → 1.7 MB UPX (−81%).

## 041b100 — granular tokio features

`tokio = { features = ["rt", "macros", "net", "time", "sync", "fs"] }`
instead of `full`: drops the dead tokio surface (process, signal, io-util,
parking_lot, …) from the binary. Final compressed size lands at 1.4 MB.

## c8794e9 — fewer allocations in the critical request path

- `SseParser::push`: zero-copy frame extraction (parse from the buffer slice
  instead of per-frame `Vec` drains).
- `assemble()`, `ResponsesToAnthropic::new()`, `collect_events()`: take `&str`
  instead of `String`.
- `server::messages()`: borrow `req.model`, clone only for the spawned
  `pump_stream` task.

No behavior change: fail-closed, terminal quota, opt-in chain intact.

## d5ed68f — concurrency and shared HTTP client

- Upstream client: connect timeout 20s→2s, 30s global timeout, 90s pool idle,
  `http2_prior_knowledge` (skips ALPN against chatgpt.com), gzip/deflate/brotli
  disabled (less CPU); `responses()` gets a 600s timeout for long streams.
- Auth client: same tuned pool for token refresh.
- Runtime: `tokio` current_thread (loopback-only, single-user).

## e046391 — release profile

`[profile.release]`: `opt-level = 3`, `lto = true`, `codegen-units = 1`,
`panic = "abort"`, `strip = true`.

## 7d9215d — picker complete regardless of upstream catalog

- `deploy/gen-model-overrides.py` always emits one row per MAIN_MODEL (7
  models) instead of omitting slugs the backend does not (yet) list in
  `/models`.
- The degraded-catalog warning now says "picker complete; inference
  unavailable until the backend exposes it" — the picker never lacks the 6
  series again.

## 0e9f313 — Codex 6 models announced in the picker

- `MAIN_SLUGS` gains `gpt-6-sol`, `gpt-6-luna`, `gpt-6.1-sol` alongside
  `gpt-6-astra` and the 5.6 line: 7 base models, one entry each; effort
  stays a request parameter, never a separate id.
- `gen-model-overrides.py` gains the three rows (label + `behavesAs` only —
  never rewrites the model sent upstream).
- A main slug missing from the current catalog still triggers the honest
  `main model missing from upstream catalog` warning.

## 4c96aae — chain fallback hardened (dual-agent review)

- The effort-per-candidate contract is pinned by a test: the selected id's
  `:xhigh` suffix must NOT carry to a candidate; the candidate body carries
  its own slug with effort re-resolved against ITS catalog entry.
- Poison-safe cooldown locks: a panicked mutex is recovered instead of
  wedging every later request (the map holds timestamps only).
- Quota detection is symmetric with the trigger guard: a quota body stops
  the walk identically on 429 and (defensively) 5xx candidate errors.
- `parse_chain` dedups order-preserving: a repeated env entry would be
  re-attempted inside one walk and open a bogus cooldown for its twin.
- Mock `fail_model` matches the parsed model field, never a substring.

## 5fc7b08 — opt-in same-provider chain fallback (`CLAUDE_CODEX_CHAIN`)

Ported from the claude-nim gateway design (ultra→super→nano chains with a 60s
per-slug cooldown). Strictly same-provider, same-account: candidates are
Codex slugs routed through the same upstream with the same auth; the
never-fall-back-to-a-paid-API contract is untouched, and an EMPTY chain keeps
behavior byte-identical.

- Trigger: selected model failed with 429/5xx that is NOT quota exhaustion.
  Quota on ANY attempt stops the walk at once and surfaces the terminal
  `retry-after: 3600` error.
- Selection: chain order, starting after the selected slug; candidates must
  resolve in the catalog or are skipped with a loud warn.
- Circuit breaker: a candidate that failed 429/5xx is skipped for 60s;
  the selected model is never keyed.
- Loud, never silent: every switch warns with original slug, failing status,
  new slug and remaining candidates; exhaustion is logged.
- 7 new integration tests + 2 env-parser unit tests.

## 5b5215d — mechanisms ported from claude-nim

- SSE ping interval 15s → 5s: shorter pings sit further inside Claude Code's
  streaming idle watchdog, so long silent reasoning phases never trip it.
- systemd tuning from the claude-nim unit: `MemoryMax=256M` and
  `MALLOC_ARENA_MAX=2` (the arena cap is what keeps RSS ~15 MB under load).
