# Gap Matrix — claudecodex (pós-fleet-fix, 2026-10-14)

Cada linha: achado da frota → estado → fix → prova.

## Corrigidos (commit 595c468)

| # | Achado | Fix | Prova |
|---|---|---|---|
| 1 | [alto] routeOf misroteava ids sufixados/fora-do-catálogo para a Anthropic | sufixo descascado do REQUEST; codex-shaped → proxy Codex (404 limpo) | simulação 10/10 + CLI real: `claude-gpt-6-astra:xhigh` → `backend_slug=gpt-6-astra` |
| 2 | [alto] timeout de catálogo 8s < janela fria 30s do proxy | `jsonT` 35s (abort cobre body) | `router.mjs` jsonT; #6 fechado junto |
| 3 | [médio] default do gerador sufixado | `claude-gpt-6-astra` base | diff linha 26 |
| 4 | [médio] launcher morria a 0ms em catálogo parcial | retry 3× cobrindo fail-TTL 10s | launcher gate |
| 5 | [baixo] POST sem limite de corpo | cap 32MB → 413 | router.mjs |
| 6 | [baixo] abort desarmado após headers (~300s undici) | `jsonT` mantém o timer até o fim do parse | router.mjs |
| 7 | [baixo] 3 round-trips + gerador não-idempotente | snapshot compartilhado + early-return-on-equal REAL | mtime congelado em launches consecutivos |
| 8 | [baixo] MODEL_FAST morta no gerador | removida | grep 1→0 |
| B | [alto] wiring output_config.effort sem teste e2e | `native_output_config_effort_flows_end_to_end` + 3 testes de precedência/mudança-de-modelo | 48 testes |
| D | [médio] log de esforço cru (forja de journal) | só níveis declarados são printáveis; hostil → `<invalid>` | server.rs printable |
| G | [médio] catálogo sem asserts herméticos | os 4 mains, sem `:`, legacy ausente-mas-resolvível — tudo por HTTP | `catalog_is_exactly_the_main_base_models_once_each` |
| K | [médio] predicado de quota duplicado | `is_quota_exhaustion` único, usado nos 2 sítios | upstream.rs/server.rs |
| O | [baixo] clamp bypassado sem metadado | guarda `KNOWN_EFFORTS` (niveis observados) | teste `invented_effort_never_reaches_upstream` |
| P | [baixo] label thinking divergente | exige `budget_tokens` | server.rs |
| U | [baixo] thinking não-codexenc descartado em silêncio | `tracing::warn` no descarte | translate.rs |

## Remanescentes

| Item | Estado | Porquê |
|---|---|---|
| Context window real | UNKNOWN | backend não expõe; nada inventado |
| PDF/document | FORA POR EVIDÊNCIA | `input_modalities` da conta = text+image; rejeição explícita |
| Refresh 401 hermético | PARCIAL | refresh bate em auth.openai.com real; coberto por `staleness_uses_eight_day_window` |
| Compaction / sessões longas | BLOCKED | exige cota de inferência (429 "usage limit" desde 2026-10-13) |
| Inferência Codex live | BLOCKED | mesma cota; fail-closed preservado (429 limpo, retry-after 3600, zero fallback) |
