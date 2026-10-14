# Repositórios auditados — Codex → Claude Code

Data: 2026-10-13 · Método: clonagem rasa + inspeção de código real (README não é prova).
Critério: recurso só conta como implementado se existe no código.

## Base escolhida e doadores

- **a1re1/claudecodex** (0★, sem LICENSE, Rust, 2.640 linhas): único que **reusa
  `~/.codex/auth.json`** (`auth.rs:74`) e tem catálogo **exato** (7 modelos = 7 aceitos
  pelo backend, verificado por inferência real). Escolhido como base.
- **pilc80/claudegpt** (33★, MIT, Rust, 24.766 linhas, 620 chamadas de teste): doador do
  mecanismo `retry_after_delay` com cap (porte + re-teste) e da distinção 4xx/5xx.
- **raine/claude-code-proxy** (646★, MIT, Rust, 80.391 linhas): OAuth próprio (login
  separado), catálogo estático que anuncia modelos que a conta rejeita (`gpt-6.1-sol`
  → 400). Referência de docs, não de catálogo.

## Descartados por evidência

- **wowyuarm/codex-proxy**: `DEFAULT_HOST = "0.0.0.0"` (`config.py:29`) — token OAuth
  exposto na LAN; sem LICENSE.
- **Aotricx/Clodex**: catálogo inflado (anuncia `gpt-6.1-sol`/`gpt-6-sol`/`gpt-6-luna`,
  que o backend não aceita para esta conta).
- **jclab-joseph/claude-code-model-proxy**: design invasivo (certificado local +
  interceptação de `api.anthropic.com`).
- **Cluster GLM** (aryan877, sjwe, powercord3, kingswordcn-create, HaroldHuanrongLIU/
  claude-proxy): todos o mesmo commit `6c21df8`, raiz `JoeInnsp23/claude-glm-wrapper` —
  wrapper GLM, não proxy Codex.
- **6 wrappers** (karem505, migueltorrezd, cobraprojects, johnlindquist, shi-rq,
  9prodhi): zero linhas de proxy próprio; delegam a terceiros.
- **insightflo, 0324wy, marashiai, rayice, VOIDXAI, PHD59fr, caixiaoshun** (conflito de
  licença GPL/MIT), **daviddawson**: sem mecanismo que o par a1re1+pilc80 não cubra.
- **StringKe/claudex**: orquestrador de perfis (escopo diferente).
- **fcakyon/claude-code-with-codex**: fork de raine (fonte: `git fork --parent`), push
  2026-10-11 — o "switch mid-conversation" vem do raine, não é independente.

## Fatos gerais

- Nenhum dos 29 tem CI.
- Catálogo REAL do backend autenticado (dump 2026-10-13): 7 modelos —
  `gpt-6-astra`, `gpt-5.6-sol/luna/terra`, `gpt-5.5` (Legacy), `gpt-reserve`,
  `codex-auto-review`; esforços declarados low/medium/high/xhigh/max(/ultra em 3).
- Licenças: a1re1 declara MIT no Cargo.toml mas **não tem arquivo LICENSE** (risco de
  redistribuição; publica nada sem resolver).
