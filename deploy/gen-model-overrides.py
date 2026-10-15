#!/usr/bin/env python3
"""Gera linhas de modelPicker para os modelos Codex na config isolada
(~/.claude-codex), a partir do catálogo autenticado.

Mecanismo comprovado no Claude Code 2.1.296: um ID que o cliente não conhece
para de emitir [claude-code:unrecognized_model] quando existe uma linha
  {"model": "<alias>", "behavesAs": "<id-anthropic>", "label": ..., "description": ...}
no modelPicker (evidência: as linhas nvidia do próprio usuário nunca warnam).
behavesAs não reescreve o model enviado — só declara capacidades.

Catálogo: usa o snapshot que o launcher acabou de baixar
(CLAUDE_CODEX_CATALOG_FILE) — zero rede própria. Sem o snapshot, busca do
roteador (fallback para execução manual).

Escrita: só regrava settings.json quando o resultado difere do estado atual —
idempotente de fato (mtime não muda em launches consecutivos).

Preserva as linhas do usuário (nvidia etc.); nunca toca o ~/.claude original.
"""
import json
import os
import sys
import urllib.request

BASE = os.environ.get("CLAUDE_CODEX_BASE", "http://127.0.0.1:4180")
TOKEN_FILE = os.path.expanduser("~/.config/claude-codex/proxy.token")
SETTINGS = os.path.expanduser("~/.claude-codex/settings.json")

# Default IGUAL ao do launcher: modelo-base, sem sufixo (esforço é parâmetro).
MODEL_MAIN = os.environ.get("CLAUDE_CODEX_MODEL", "claude-gpt-6-astra")

# Modelos principais, um por modelo-base (curadoria confirmada contra o
# catálogo REAL do backend: astra=frontier, sol=workhorse, luna=fast,
# terra=balanced; 5.5 é "Legacy", reserve e auto-review são auxiliares —
# fora do seletor). NÃO há linhas por esforço.
MAIN_MODELS = {
    "gpt-6-astra":   ("GPT-6 Astra",   "claude-opus-4-8"),
    "gpt-6-sol":     ("GPT-6 Sol",     "claude-sonnet-5"),
    "gpt-6-luna":    ("GPT-6 Luna",    "claude-haiku-4-5-20251001"),
    "gpt-6.1-sol":   ("GPT-6.1 Sol",   "claude-opus-4-8"),
    "gpt-5.6-sol":   ("GPT-5.6 Sol",   "claude-sonnet-5"),
    "gpt-5.6-terra": ("GPT-5.6 Terra", "claude-sonnet-4-6"),
    "gpt-5.6-luna":  ("GPT-5.6 Luna",  "claude-haiku-4-5-20251001"),
}


def load_catalog():
    """Snapshot do launcher primeiro; rede só como fallback manual."""
    snap = os.environ.get("CLAUDE_CODEX_CATALOG_FILE")
    if snap and os.path.isfile(snap):
        try:
            with open(snap) as f:
                return json.load(f)
        except Exception:
            pass  # snapshot ilegível -> cai no fetch
    try:
        with open(TOKEN_FILE) as f:
            token = f.read().strip()
        req = urllib.request.Request(
            f"{BASE}/v1/models?limit=1000",
            headers={"Authorization": f"Bearer {token}"},
        )
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.load(r)
    except Exception as e:
        print(f"gen-model-picker: catálogo indisponível ({e}); mantendo atual",
              file=sys.stderr)
        return None


def main() -> int:
    data = load_catalog()
    catalog_ids: set[str] = set()
    if data is not None:
        catalog_ids = {m.get("id", "") for m in data.get("data", [])}

    # Sempre emite uma linha por MAIN_MODEL, independentemente do catálogo
    # upstream já expor o slug. O picker do Claude Code só precisa da row
    # (behavesAs declara capacidades; o model enviado é o slug Codex). O
    # aviso abaixo nomeia honestamente o que ainda não está disponível para
    # inferência — sem deixar o picker incompleto.
    rows = []
    for base in MAIN_MODELS:
        label, behaves = MAIN_MODELS[base]
        full_id = f"claude-{base}"
        rows.append({
            "model": full_id,
            "label": label,
            "behavesAs": behaves,
            "description": "ChatGPT Pro via Codex — esforço ajustável no controle nativo",
        })

    missing = [base for base in MAIN_MODELS
               if f"claude-{base}" not in catalog_ids]
    if missing:
        # Fleet finding: catálogo parcial degradava em silêncio através de
        # todas as camadas. Agora é nomeado, alto e claro. O picker fica
        # COMPLETO mesmo assim — apenas a inferência nos modelos ausentes
        # falhará até o backend os expor.
        print(f"gen-model-picker: AVISO — modelo(s) principal(is) ausente(s) do catálogo: "
              f"{', '.join(missing)} (picker completo; inferência indisponível até o backend expor)",
              file=sys.stderr)

    if not rows:
        print("gen-model-picker: nenhum modelo principal configurado; nada a fazer",
              file=sys.stderr)
        return 0

    with open(SETTINGS) as f:
        settings = json.load(f)
    # Ensure 0600 on every load (idempotent), not only on write. Cycle-3
    # #17: stat/chmod follow symlinks — on a symlinked settings.json, warn
    # and skip both the chmod and the write (fail-safe, no traversal).
    if os.path.islink(SETTINGS):
        print("gen-model-picker: AVISO — settings.json é symlink; não chmodo nem regravo através dele",
              file=sys.stderr)
    else:
        try:
            if (os.stat(SETTINGS).st_mode & 0o777) != 0o600:
                os.chmod(SETTINGS, 0o600)
        except OSError:
            pass

    # Estado desejado, calculado por completo antes de qualquer escrita.
    picker = settings.get("modelPicker") or {}
    if not isinstance(picker, dict):
        picker = {}
    user_rows = [
        r for r in (picker.get("options") or [])
        if not (str(r.get("model", "")).startswith("claude-gpt-")
                or str(r.get("model", "")).startswith("claude-codex-"))
    ]
    desired_options = user_rows + rows
    env = dict(settings.get("env") or {})
    # Gates de discovery comprovados: a assertion first-party bloqueia o
    # discovery; USE_COWORK_PLUGINS derrota o behavesAs (bisseção V1).
    env.pop("CLAUDE_CODE_USE_COWORK_PLUGINS", None)
    env.pop("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", None)

    # Idempotência REAL: se nada muda, não escreve (mtime preservado).
    if (settings.get("modelPicker", {}).get("options") == desired_options
            and settings.get("model") == MODEL_MAIN
            and settings.get("env") == env
            and "modelOverrides" not in settings):
        return 0

    picker["options"] = desired_options
    settings["modelPicker"] = picker
    settings["model"] = MODEL_MAIN
    settings["env"] = env
    settings.pop("modelOverrides", None)

    # Fleet finding: open() segue symlink e o arquivo ficava 644. Agora:
    # O_CREAT|O_EXCL|O_NOFOLLOW no tmp e 0600 no tmp E no destino.
    tmp = SETTINGS + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(settings, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, SETTINGS)
    os.chmod(SETTINGS, 0o600)
    print(f"gen-model-picker: {len(rows)} linhas codex no picker", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
