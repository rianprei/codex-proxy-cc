# Arquitetura — claudecodex (pós-restauração, commit 595c468)

```
claude (launcher, ~/.local/bin/claude)
  ├─ healthz + catálogo (retry 3×) + snapshot → gerador (4 linhas de picker)
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
  → Codex backend (retry 401×1, 429/5xx×2; quota=terminal, retry-after 3600)
  → log redigido: selected_model → backend_slug → effort_received(fonte) → effort_sent
```

Decisões com prova:
- Esforço é parâmetro do request (`output_config.effort`), nunca id de modelo:
  um por base, 4 no seletor; sufixo `:effort` só por API direta.
- Discovery oficial do Claude Code (`GET /v1/models?limit=1000`, 3s default):
  o cache dedicado é regenerado por launch porque o 2.1.296 com cache presente
  não revalida e os modelos ficam não-registrados.
- Gates de discovery comprovados: sem `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`,
  sem `CLAUDE_CODE_USE_COWORK_PLUGINS` (derrota o behavesAs — bisseção V1);
  sem `CLAUDE_CODE_USE_COWORK_PLUGINS` os plugins do store principal carregam
  MAIS (26 vs 20 skills).
- Fail-closed em toda a cadeia: 429 de quota = erro terminal, zero fallback
  para Anthropic/NIM; ids codex-shaped não conhecidos → proxy Codex (404 limpo).

Limites declarados: context window UNKNOWN (backend não expõe); PDF/document
rejeitado explicitamente (`input_modalities` = text+image).
