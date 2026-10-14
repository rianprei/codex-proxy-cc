#!/usr/bin/env bash
# Regression suite de campo — bugs realmente observados, não hipóteses.
# Recriado após o wipe de /tmp (fleet re-review #1: o arquivo se perdeu na
# restauração e o test-plan citava um comando inexistente).
set -uo pipefail

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n    %s\n' "$1" "$2"; FAIL=$((FAIL+1)); }

CCX="$HOME/.local/bin/claude"
ROUTER="http://127.0.0.1:4180"
TOKEN_FILE="$HOME/.config/claude-codex/proxy.token"

echo "== regressao: comando e dispatcher =="
[ ! -e "$HOME/.local/bin/claude-codex" ] \
  && ok "comando claude-codex removido (tudo via claude)" \
  || bad "claude-codex ainda existe" "remova ~/.local/bin/claude-codex"
grep -q 'CLAUDE_REAL_BIN=' "$CCX" \
  && ok "claude executa o binário real (sem loop)" \
  || bad "claude sem CLAUDE_REAL_BIN" "exec tem que usar o binário real"

echo; echo "== regressao: credencial Codex unica =="
[ -f "$HOME/.codex/auth.json" ] && ok "auth.json existe" || bad "auth.json ausente" "codex login"
[ ! -f "$HOME/.config/claude-code-proxy/codex/auth.json" ] \
  && ok "nenhum OAuth secundário" \
  || bad "existe credencial separada do proxy" "viola reutilizar a sessão"

echo; echo "== regressao: roteador :4180 =="
TOK="$(cat "$TOKEN_FILE" 2>/dev/null || true)"
HDR="$(mktemp)"; printf 'Authorization: Bearer %s\n' "$TOK" > "$HDR"; chmod 600 "$HDR"
trap 'rm -f "$HDR"' EXIT
if curl -fsS --max-time 3 "$ROUTER/healthz" >/dev/null 2>&1; then
  C=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$ROUTER/v1/models")
  [ "$C" = "401" ] && ok "/v1/models sem token -> 401" || bad "sem token -> $C" "esperado 401"
  C=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST "$ROUTER/v1/messages" -H 'Content-Type: application/json' -d '{"model":"claude-sonnet-5"}')
  [ "$C" = "401" ] && ok "/v1/messages sem token -> 401" || bad "sem token -> $C" "esperado 401"
  C=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$ROUTER/v1/models" -H @"$HDR" -H 'Host: evil.com')
  [ "$C" = "403" ] && ok "Host de rebinding -> 403" || bad "Host evil -> $C" "esperado 403"
  C=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST "$ROUTER/v1/complete" -H @"$HDR" -H 'Content-Type: application/json' -d '{}')
  [ "$C" = "404" ] && ok "path fora da allowlist -> 404" || bad "path livre -> $C" "esperado 404"
  N=$(curl -fsS --max-time 40 "$ROUTER/v1/models?limit=1000" -H @"$HDR" 2>/dev/null | python3 -c '
import json,sys
try:
    d=json.load(sys.stdin)["data"]
    c=[m for m in d if "gpt-" in m.get("id","") or "codex-" in m.get("id","")]
    print(f'{len(c)}:{sum(1 for m in c if ":" in m["id"])}')
except Exception: print("0:1")')
  CCNT="${N%%:*}"; CCOL="${N##*:}"
  [ "${CCNT:-0}" = "4" ] && [ "${CCOL:-1}" = "0" ] \
    && ok "catálogo: 4 modelos-base, 0 variantes de esforço" \
    || bad "catálogo errado (base=$CCNT com-esforço=$CCOL)" "esperado 4/0"
  grep -q 'CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1' "$CCX" \
    && ok "launcher ativa gateway discovery" \
    || bad "launcher sem discovery" "o /model não popula via gateway"
  grep -q -- '--model "$MODEL_MAIN" "$@"' "$CCX" \
    && ok "launcher injeta --model (sem revert silencioso)" \
    || bad "launcher sem --model" "default pode reverter para a Anthropic"
  grep -q 'H @' "$CCX" \
    && ok "token fora do argv (header-file)" \
    || bad "token no argv do curl" "/proc/*/cmdline expõe"
else
  bad "roteador :4180 fora" "systemctl --user start claude-multirouter"
fi

echo; echo "== regressao: settings isolado =="
if python3 - <<'PY'
import json,os,sys
s=json.load(open(os.path.expanduser('~/.claude-codex/settings.json')))
env=s.get('env',{})
rows=(s.get('modelPicker') or {}).get('options') or []
assert '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL' not in env, "assertion first-party presente"
assert 'CLAUDE_CODE_USE_COWORK_PLUGINS' not in env, "USE_COWORK_PLUGINS mata o behavesAs"
codex=[r for r in rows if str(r.get('model','')).startswith('claude-gpt-')]
assert len(codex)==4, f"picker com {len(codex)} linhas codex (esperado 4)"
assert all(':' not in r['model'] for r in codex), "linha com sufixo de esforço"
assert s.get('model','').startswith('claude-gpt-'), "model default não é o alias"
PY
then ok "settings: gates + 4 linhas Codex + default no alias"
else bad "settings isolado inválido" "confira env/modelPicker/model"
fi

echo; echo "== regressao: token em logs =="
LOGS=$(journalctl --user -u claudecodex.service --no-pager -n 200 2>/dev/null; journalctl --user -u claude-multirouter.service --no-pager -n 200 2>/dev/null)
# Cycle-3 #23: empty journal previously passed as "no token found" (fail-open)
# and the bare LOCAL proxy token matched no pattern class. Require a non-empty
# subject and grep the literal token value (stays on this machine).
if [ -z "$LOGS" ]; then
  bad "journal indisponível/vazio" "não é possível afirmar ausência de token sem examinar linhas"
elif echo "$LOGS" | grep -qEi 'eyJ[A-Za-z0-9_-]{20,}|rt\.1\.|Bearer [A-Za-z0-9]{20,}'; then
  bad "token no log" "verifique journalctl"
elif [ -s "$TOKEN_FILE" ] && echo "$LOGS" | grep -qF "$(cat "$TOKEN_FILE")"; then
  bad "token local do proxy apareceu cru no journal" "verifique journalctl"
else ok "nenhum token em 400 linhas de journal"
fi

echo; echo "== regressao: fallback pago e preservação =="
for W in "$CCX"; do
  grep -qE 'unset .*OPENAI_API_KEY' "$W" \
    && ok "$(basename "$W") remove OPENAI_API_KEY do filho" \
    || bad "$(basename "$W") não limpa OPENAI_API_KEY" "risco de fallback"
done
[ -f "$HOME/.local/bin/claude-pinned" ] \
  && ok "wrapper original preservado (claude-pinned)" \
  || bad "claude-pinned sumiu" "backup do wrapper"
BASE="$HOME/.config/claude-codex/settings.baseline.sha256"
# Cycle-3 #11: a missing file produced an empty SUM and the first run
# registered a VOID baseline (""=="" forever green). Fail loudly instead.
[ -f "$HOME/.claude/settings.json" ] \
  || bad "settings.json do usuário AUSENTE" "a pior violação da preservação — investigue antes de prosseguir"
SUM=$(sha256sum "$HOME/.claude/settings.json" 2>/dev/null | awk '{print $1}')
[ -n "$SUM" ] || bad "hash vazio para settings.json" "sha256sum falhou"
if [ -f "$BASE" ]; then
  if [ "$SUM" = "$(cat "$BASE")" ]; then
    # Known limitation (cycle-3 #22): TOFU — the baseline lives beside the
    # guarded file, writable by the same user; this detects accidental drift,
    # not same-user tampering (that user could edit this script too).
    ok "settings.json do usuário byte-idêntico ao baseline (TOFU: drift acidental)"
  else
    bad "settings.json mudou" "esperado $(cat "$BASE"), obtido $SUM"
  fi
else
  mkdir -p "$(dirname "$BASE")"; printf '%s' "$SUM" > "$BASE"
  ok "baseline de settings.json registrado"
fi
PERM=$(stat -c %a "$HOME/.claude-codex/settings.json" 2>/dev/null)
[ "$PERM" = "600" ] && ok "settings isolado 0600" || bad "settings $PERM" "esperado 600"

echo; echo "================================"
printf '  %d passaram, %d falharam\n' "$PASS" "$FAIL"
echo "================================"
[ "$FAIL" -eq 0 ]
