# Arquitetura — claudecodex (pós-profissionalização, commit 2ab6e2c)

```
claude (launcher, ~/.local/bin/claude) — mkdir antes do mktemp do header-file 0600
  (token fora do argv, removido antes do exec; sweep de vazamentos antigos)
  ├─ healthz + catálogo (retry 3×; gate exige ≥4 modelos-base; snapshot
  │  atômico tmp+mv) → gerador (4 linhas de picker; stderr visível)
  └─ exec binário real --model claude-gpt-6-astra "$@"
        │  env: CLAUDE_CONFIG_DIR=~/.claude-codex, BASE_URL=:4180,
        │       GATEWAY_MODEL_DISCOVERY=1, discovery cache regenerado por launch
        ▼
claude-multirouter :4180 (auth local: bearer=proxy.token; loopback+Host check;
  cap 32MB; catálogo cacheado 60s/10s-fail, jsonT 35s)
  ├─ claude-gpt-* / claude-codex-* → claudecodex :18766
  ├─ claude-nvidia-*               → nim gateway :4111
  └─ demais claude-*               → api.anthropic.com (OAuth do usuário)
        ▼
claudecodex (Rust): validate_content_blocks → resolve (MAIN_SLUGS, um por base)
  → choose_effort(request > sufixo > thinking > default, clamp ao declarado)
  → auth.rs: credenciais de ~/.codex/auth.json; refresh proativo quando
    last_refresh > 8 dias, refresh forçado após 401 (com guard de token
    rejeitado — nunca duas trocas pela mesma rejeição); auth.json regravado
    com escrita atômica (tmp 0600 + rename), campos desconhecidos preservados
  → Codex backend (retry 401×1, 429/5xx×2; quota=terminal, retry-after 3600)
  → log redigido: selected_model → backend_slug → effort_received(fonte) → effort_sent
```

Decisões com prova:
- Esforço é parâmetro do request (`output_config.effort`), nunca id de modelo:
  um por base, 7 no seletor (astra, 6-sol, 6-luna, 6.1-sol, 5.6-sol,
  5.6-terra, 5.6-luna — curadoria do que o backend descreve como lineup
  principal, cada um verificado por inferência real); sufixo `:effort` só
  por API direta. Legacy/reserve/auxiliares resolvem por id direto, não são
  anunciados; slug main ausente do catálogo do momento segue no picker com
  aviso (7d9215d/0e9f313).
- Discovery oficial do Claude Code (`GET /v1/models?limit=1000`, 3s default):
  o cache dedicado é regenerado por launch porque o 2.1.296 com cache presente
  não revalida e os modelos ficam não-registrados.
- Gates de discovery comprovados: sem `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`,
  sem `CLAUDE_CODE_USE_COWORK_PLUGINS` (derrota o behavesAs — bisseção V1);
  sem `CLAUDE_CODE_USE_COWORK_PLUGINS` os plugins do store principal carregam
  MAIS (26 vs 20 skills).
- Fail-closed em toda a cadeia: 429 de quota = erro terminal, zero fallback
  para Anthropic/NIM; ids codex-shaped não conhecidos → proxy Codex (404 limpo).
  Opt-in (`CLAUDE_CODEX_CHAIN`): fallback estritamente same-provider/same-account
  — slugs Codex em ordem de preferência, disparado só por 429/5xx não-quota,
  quota nunca encadeia (terminal imediato, no selecionado OU em candidato),
  cooldown de 60s por slug candidato, cada switch em `warn` (original, status,
  novo slug, restantes); vazio = desligado; parse tolerante (trim, vazios e
  duplicatas descartados — 4c96aae); effort re-resolvido por candidato contra
  a entrada de catálogo do PRÓPRIO candidato (o sufixo `:xhigh` do id
  selecionado não carrega).
- Binário enxuto (e046391 → 2ab6e2c): 10 deps diretas (anyhow/chrono/clap/
  tracing/uuid/env_logger/futures-util substituídos por ~175 linhas std-only
  em util.rs + logger stderr de ~35 linhas), axum `default-features=false`
  (só http1/json/tokio — remove a árvore transitiva de tracing), futures-core
  no lugar de futures-util, tokio com features granulares, reqwest sem
  features default. Perfil release: opt-level 3, LTO, codegen-units=1,
  panic=abort, strip. 8,8M ingênuo → 3,7M release → 1,4M UPX.

Limites declarados: context window UNKNOWN (backend não expõe); PDF/document
rejeitado explicitamente (`input_modalities` = text+image).
