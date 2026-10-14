# Plano de testes — claudecodex (commit 595c468)

## Camada 1 — unitários (in-src): 19
Parser SSE, shape Anthropic, round-trip auth, alias/effort (incl. `ultracode`
→ clamp), OutputConfig parse, catálogo um-por-base, sem duplicatas,
legacy-resolvível, exaustão de esforço inventado sem metadado.

## Camada 2 — integração hermética (tests/integration.rs): 27
Mock axum em porta efêmera; codex-home falso em tmp; zero credencial real.
- Protocolo: ordem message_start…message_stop, índices monotônicos, texto em
  deltas, assembly de tool args em JSON válido, system/multi-turn, tool_result
- Catálogo: **exatamente os 4 mains, uma vez cada, sem `:`**; legacy ausente
  do catálogo MAS resolvível por POST; todos com prefixo claude-
- Esforço: sufixo→upstream; **output_config.effort e2e**; nativo vence sufixo;
  mudar esforço nunca muda base; inventado nunca chega ao backend
- Erros: 429→rate_limit, 403→authentication, 400→invalid_request;
  429 transitório recupera (3 chamadas), 5xx exaure (3), 400 não retria;
  quota não retria + retry-after>60; Retry-After com cap 20s/fallback 2s
- Regressões: document block → 400 nomeando o tipo, 0 hits; system idem;
  blocos suportados passam; count_tokens local (0 hits)
- Auth: 401 sem/com token errado (0 hits), 200 com correto
- Concorrência: 8 sessões isoladas

## Camada 3 — regressão de campo (tests/regression.sh): 25 checks
Dispatcher sem loop de exec, catálogo 4-base/0-variantes, auth 401/401/200,
Host 403, path 404, token em log 0, picker 4 linhas sem `:`, defaults,
settings byte-a-baseline, launcher discovery+`--model` injetado.

## Camada 4 — live
`claude` → 4 modelos no seletor, esforço nativo (journal: request:low/high/medium),
sufixo documentado → Codex (`backend_slug=gpt-6-astra`, 429 de quota), NIM e
Claude com inferência real. **Inferência Codex: BLOCKED pela cota** ("usage limit
has been reached" desde 2026-10-13); fail-closed preservado.

## Comandos
```bash
cargo test                                   # 46 verdes
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # limpo/0
bash tests/regression.sh                     # 25 verdes
claude; claude --help; claude -p "Responda exatamente OK"
```
