# Deployment layer

Como esta máquina roda o `claude` no backend Codex da assinatura ChatGPT Pro:

- `claude-launcher` → `~/.local/bin/claude` — wrapper do comando `claude`:
  sobe/verifica o roteador, gate de catálogo (≥4 modelos-base), descoberta
  oficial de gateway, injeta `--model` base e limpa variáveis de provedores
  pagos. O binário original fica em `claude-pinned`.
- `claude-multirouter.mjs` → `~/.local/share/claude-multirouter/router.mjs` —
  roteador Anthropic-compatible em `127.0.0.1:4180`: despacha por nome de
  modelo (Codex / NIM / Anthropic nativo com o OAuth do usuário), auth de
  cliente obrigatória, Host check anti-rebinding, cap de 32MB, abort do
  cliente propagado ao upstream.
- `gen-model-overrides.py` → mesmo diretório — gera as 4 linhas do
  modelPicker na config isolada a partir do catálogo autenticado;
  idempotente (só regrava quando muda).

Autenticação: somente leitura de `~/.codex/auth.json` (sessão existente do
Codex CLI). Nenhum OAuth adicional, nenhuma API paga, nenhum segredo em
argv/logs — token local do proxy em arquivo 0600.

Serviços (systemd --user): `claudecodex.service` (o proxy deste repo) e
`claude-multirouter.service` (o roteador), ambos loopback-only.
