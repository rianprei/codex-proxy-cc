#!/usr/bin/env node
// Roteador Anthropic-compatible: fronts Claude (nativo), Codex (ChatGPT Pro) e NIM
// numa unica porta, despachando pelo nome do modelo. Sem fallback automatico:
// cada modelo vai para exatamente um upstream.
//
//   claude-gpt-* , gpt-* , codex-*   ->  claudecodex   127.0.0.1:18766
//   claude-nvidia-* , nvidia-*       ->  nim gateway   127.0.0.1:4111
//   demais claude-*                  ->  api.anthropic.com (OAuth do Claude Code)
//
// Bind: 127.0.0.1 apenas. Todo endpoint de API exige o token local do cliente
// (o mesmo proxy.token que o launcher exporta como ANTHROPIC_AUTH_TOKEN).

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const PORT = Number(process.env.ROUTER_PORT || 4180);
const HOST = "127.0.0.1";
const CODEX_UP = "http://127.0.0.1:18766";
const NIM_UP = "http://127.0.0.1:4111";
const ANTHROPIC_UP = "https://api.anthropic.com";

const HOME = os.homedir();
const readToken = (p) => { try { return fs.readFileSync(p, "utf8").trim(); } catch { return null; } };
const CODEX_TOKEN = readToken(path.join(HOME, ".config/claude-codex/proxy.token"));

function nimKey() {
  try {
    const raw = fs.readFileSync(path.join(HOME, ".config/nvidia/proxy.env"), "utf8");
    const m = raw.match(/^LITELLM_MASTER_KEY=(.*)$/m);
    return m ? m[1].trim().replace(/^["']|["']$/g, "") : null;
  } catch { return null; }
}

/// Claude OAuth from the user's own credential store. Read fresh on every
/// request: Claude Code rotates this token, and a cached copy would go stale.
/// Never logged, never printed.
function claudeOAuth() {
  try {
    const raw = fs.readFileSync(path.join(HOME, ".claude/.credentials.json"), "utf8");
    const j = JSON.parse(raw);
    return j?.claudeAiOauth?.accessToken || null;
  } catch { return null; }
}

// Routing por MEMBRO do catalogo vivo primeiro; a regex e so fallback para
// o instante anterior ao primeiro catalogo. Um slug novo do backend (ex.:
// "o4-mini" -> "claude-o4-mini") roteia certo pelo catalogo, nao pelo padrao.
let routed = { codex: new Set(), nim: new Set() };
function updateRouteSets(data) {
  routed.codex = new Set(data.filter((m) => m.provider === "codex").map((m) => m.id));
  routed.nim = new Set(data.filter((m) => m.provider === "nim").map((m) => m.id));
}

function routeOf(model) {
  const m = String(model || "");
  // O sufixo :effort é parâmetro do REQUEST (o catálogo só tem ids base):
  // descasca do lado do pedido, nunca do lado do catálogo. O código antigo
  // descascava o id do CATÁLOGO — inverteu a premissa quando o catálogo
  // virou só-base e mandou ids sufixados pra Anthropic (misroute V1).
  const base = m.split(":")[0];
  // Cycle-3 #20: each set is consulted independently — a codex outage must
  // not disable nim membership routing.
  if (routed.codex.size > 0 && (routed.codex.has(m) || routed.codex.has(base))) return "codex";
  if (routed.nim.size > 0 && (routed.nim.has(m) || routed.nim.has(base))) return "nim";
  // Default por FORMA: id com cara de Codex vai para o proxy Codex (404
  // limpo se inválido). NUNCA para a Anthropic com o OAuth do usuário.
  const l = base.toLowerCase();
  if (/^(claude-)?(gpt-|codex-)/.test(l)) return "codex";
  if (/^(claude-)?nvidia-/.test(l)) return "nim";
  // Cycle-3 #8: the anthropic branch is for REAL claude-* model ids only —
  // garbage ("", "banana", "claude-") must be rejected, not routed with the
  // user's OAuth.
  if (/^claude-[a-z0-9]/.test(l)) return "anthropic";
  return "reject";
}

const UPSTREAM = { codex: CODEX_UP, nim: NIM_UP, anthropic: ANTHROPIC_UP };

/// fetch com timeout: sem isto o undici espera 300s num socket preso.
async function fetchT(url, opts = {}, ms = 8000) {
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), ms);
  // Fleet finding: um signal externo (abort do cliente) era silenciosamente
  // sobrescrito pelo interno — propaga ambos.
  const external = opts.signal;
  const onAbort = () => ac.abort();
  if (external) external.addEventListener("abort", onAbort, { once: true });
  try {
    return await fetch(url, { ...opts, signal: ac.signal });
  } finally {
    clearTimeout(t);
    // Cycle-3 #1 (alto): removing the external listener here cuts the abort
    // bridge the moment headers arrive — the body phase becomes unabortable.
    // KEEP the listener: the client's signal dies with the request and is
    // GC'd with it, so this is not a leak. The bridge now lives for the
    // whole body phase, which is what fix #4 always claimed.
  }
}

/// fetch + parse de JSON com o timeout cobrindo ATÉ o fim do corpo: o
/// clearTimeout de fetchT desarma quando os headers chegam, e um upstream
/// que envia headers e encalha o corpo deixaria o undici esperar ~300s
/// com catalogInflight preso (achado V2 da frota).
async function jsonT(url, opts = {}, ms = 8000) {
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), ms);
  try {
    const r = await fetch(url, { ...opts, signal: ac.signal });
    return await r.json();
  } finally {
    clearTimeout(t);
  }
}

function headersFor(kind, incoming) {
  const h = {};
  for (const k of ["anthropic-version", "anthropic-beta", "x-api-key", "accept", "content-type"])
    if (incoming[k]) h[k] = incoming[k];
  h["content-type"] = "application/json";
  if (kind === "codex") {
    h["authorization"] = `Bearer ${CODEX_TOKEN}`;
  } else if (kind === "nim") {
    const k = nimKey();
    if (k) h["authorization"] = `Bearer ${k}`;
  } else {
    const tok = claudeOAuth();
    if (tok) {
      h["authorization"] = `Bearer ${tok}`;
      h["anthropic-beta"] = [incoming["anthropic-beta"], "oauth-2025-04-20"]
        .filter(Boolean)
        .join(",");
    } else if (incoming["x-api-key"]) {
      h["x-api-key"] = incoming["x-api-key"];
    }
  }
  return h;
}

// ---------------------------------------------------------------------------
// Catalogo com cache: o discovery do Claude Code tem timeout de 3s e falha
// silenciosamente. Cache OK vale 60s; cache parcial (fetch codex falhou) vale
// so 10s e nunca substitui uma lista boa que ja esteja em cache. Single-flight:
// requests concorrentes compartilham o mesmo refresh.
// ---------------------------------------------------------------------------
const CATALOG_OK_TTL_MS = 60_000;
const CATALOG_FAIL_TTL_MS = 10_000;
let catalogCache = { data: [], at: 0, ok: false };
let catalogInflight = null;

/// Extrai o valor do primeiro campo "model" do corpo JSON, tratando escapes.
/// Limita o scan aos primeiros 64KB (o campo vem no comeco); retorna null se
/// nao encontrou — o chamador cai no parse completo como fallback.
function extractModel(raw) {
  const SCAN_LIMIT = 64 * 1024;
  const head = raw.length > SCAN_LIMIT ? raw.slice(0, SCAN_LIMIT) : raw;
  const m = head.match(/"model"\s*:\s*"/);
  if (!m) return null;
  let i = m.index + m[0].length, out = "";
  while (i < head.length && out.length < 256) {
    const c = head[i];
    if (c === '"') return out;
    if (c === "\\") {
      // Cycle-3 #5: the manual unescape is lossy for \uXXXX — bail out and
      // let the full JSON.parse (which decodes correctly) decide.
      return null;
    }
    out += c;
    i++;
  }
  return null; // sem fechamento no limite — deixa o parse completo decidir
}

async function collectModels() {
  const out = [];
  let codexOk = false;
  // Repassa o display_name do catalogo de cada upstream (o proxy ja cura
  // nomes limpos, um por modelo-base); so marca o provedor quando o upstream
  // nao mandou nome.
  const push = (entries, provider) => {
    for (const e of entries) {
      out.push({
        type: "model",
        id: e.id,
        display_name: e.display_name && e.display_name !== e.id ? e.display_name : `${e.id} [${provider}]`,
        provider,
      });
    }
  };
  push(["claude-opus-5", "claude-opus-4-8", "claude-sonnet-5", "claude-sonnet-4-6",
        "claude-haiku-4-5-20251001", "claude-fable-5"].map((id) => ({ id })), "anthropic");
  try {
    // 35s: casado com o orçamento de 30s do proxy para o fetch FRIO do
    // catálogo (medido até 7.9s em conta throttled) — 8s abortava dentro
    // da janela real e falsificava codexOk=false em restart acoplado.
    const j = await jsonT(`${CODEX_UP}/v1/models`, { headers: { authorization: `Bearer ${CODEX_TOKEN}` } }, 35_000);
    const entries = (j.data || []).map((m) => ({ id: m.id, display_name: m.display_name }));
    if (entries.length > 0) {
      push(entries, "codex");
      codexOk = true;
    }
  } catch {}
  const k = nimKey();
  try {
    const r = await jsonT(`${NIM_UP}/v1/models`, { headers: k ? { authorization: `Bearer ${k}` } : {} });
    push((r.data || []).map((m) => ({ id: m.id, display_name: m.display_name })), "nim");
  } catch {}
  return { data: out, codexOk };
}

function catalogFresh(now) {
  const ttl = catalogCache.ok ? CATALOG_OK_TTL_MS : CATALOG_FAIL_TTL_MS;
  return catalogCache.data.length > 0 && now - catalogCache.at < ttl;
}

/// Serve SEMPRE em tempo de cache: se existe lista (mesmo vencida), devolve
/// na hora e revalida por tras. O discovery do Claude Code tem timeout de 3s
/// e FALHA EM SILENCIO — um fan-out frio de ~2-9s no upstream esvaziava o
/// picker sem uma linha de log. So bloqueia quando nao ha lista nenhuma.
function modelsPayload() {
  const now = Date.now();
  if (catalogCache.data.length > 0) {
    if (!catalogFresh(now) && !catalogInflight) {
      // revalida por tras; a resposta sai agora com o que ja temos
      catalogInflight = (async () => {
        const { data, codexOk } = await collectModels();
        const t = Date.now();
        if (codexOk) {
          catalogCache = { data, at: t, ok: true };
        } else if (!catalogCache.ok) {
          catalogCache = { data, at: t, ok: false };
        }
        updateRouteSets(catalogCache.data);
        return catalogCache.data;
      })().finally(() => { catalogInflight = null; });
    }
    return Promise.resolve(catalogCache.data);
  }
  // Primeira carga (boot): bloqueia; o pre-warm do listen() ja dispara isto.
  if (catalogInflight) return catalogInflight;
  catalogInflight = (async () => {
    const { data, codexOk } = await collectModels();
    catalogCache = { data, at: Date.now(), ok: codexOk };
    updateRouteSets(catalogCache.data);
    return catalogCache.data;
  })().finally(() => { catalogInflight = null; });
  return catalogInflight;
}

// ---------------------------------------------------------------------------
const server = http.createServer(async (req, res) => {
  // Loopback-only, e Host tem que ser local: bind + remoteAddress nao
  // bloqueiam DNS rebinding (uma pagina publica que resolve pra 127.0.0.1).
  const remote = req.socket.remoteAddress || "";
  const host = String(req.headers.host || "").split(":")[0].toLowerCase();
  if (!/^(::1|::ffff:127\.0\.0\.1|127\.0\.0\.1)$/.test(remote) ||
      (host !== "127.0.0.1" && host !== "localhost" && host !== "")) {
    res.writeHead(403).end("loopback only");
    return;
  }

  if (req.method === "GET" && (req.url === "/healthz" || req.url === "/")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true, service: "claude-multirouter", port: PORT }));
    return;
  }

  // Autenticacao do cliente: o mesmo proxy.token que o launcher exporta.
  // Sem isto, qualquer processo local usaria as tres credenciais sem segredo.
  const bearer = String(req.headers.authorization || "").replace(/^Bearer /i, "");
  const apiKey = String(req.headers["x-api-key"] || "");
  const authed = CODEX_TOKEN && (bearer === CODEX_TOKEN || apiKey === CODEX_TOKEN);

  if (req.method === "GET" && req.url.startsWith("/v1/models")) {
    if (!authed) { res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ type: "error", error: { type: "authentication_error", message: "invalid or missing auth token" } }));
      return; }
    const data = await modelsPayload();
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ data }));
    return;
  }

  // Connection-warming probe que o Claude Code envia na inicializacao.
  if (req.method === "HEAD" && req.url === "/api/hello") {
    res.writeHead(200).end();
    return;
  }

  if (req.method !== "POST") return res.writeHead(405).end();

  // Allowlist de path: o OAuth do usuario nao pode ser carregado para
  // endpoint arbitrario do upstream.
  const p = req.url.split("?")[0];
  if (p !== "/v1/messages" && p !== "/v1/messages/count_tokens") {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ type: "error", error: { type: "not_found_error", message: `path not allowed: ${p}` } }));
    return;
  }
  if (!authed) {
    res.writeHead(401, { "content-type": "application/json" });
    res.end(JSON.stringify({ type: "error", error: { type: "authentication_error", message: "invalid or missing auth token" } }));
    return;
  }

  // Cap de corpo: o proxy Rust tem o DefaultBodyLimit de 2MB do axum; sem
  // isto o router bufferizava POSTs arbitrários em RAM (achado V1) — 32MB
  // é folga sobre o teto real do cliente, e estourar responde 413.
  const MAX_BODY_BYTES = 32 * 1024 * 1024;
  const chunks = [];
  let bodySize = 0;
  for await (const c of req) {
    bodySize += c.length;
    if (bodySize > MAX_BODY_BYTES) {
      res.writeHead(413, { "content-type": "application/json" });
      res.end(JSON.stringify({ type: "error", error: { type: "request_too_large", message: `request body exceeds ${MAX_BODY_BYTES} bytes` } }));
      return;
    }
    chunks.push(c);
  }
  const raw = Buffer.concat(chunks).toString("utf8");

  // Extracao dirigida do campo .model (fleet finding: parse completo do corpo
  // travava o event loop por dezenas de ms em sessoes com imagens). O campo
  // "model" vem no comeco do corpo do Claude Code; scan limitado com
  // fallback para o parse completo quando o scan nao acha.
  let model = extractModel(raw);
  if (model === null) {
    try { model = JSON.parse(raw).model || ""; } catch { model = ""; }
  }
  const kind = routeOf(model);
  if (kind === "reject") {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ type: "error", error: { type: "not_found_error", message: `unsupported model id: ${JSON.stringify(model)}` } }));
    return;
  }
  const target = `${UPSTREAM[kind]}${req.url}`;

  // Abort do cliente propaga pro upstream: sem isto, um ESC no Claude Code
  // deixa o router bombeando (e queimando cota) ate o fim da geracao.
  // O signal agora viaja opts.signal ate o fetch (fleet finding #4).
  const ac = new AbortController();
  req.socket.on("close", () => { if (!res.writableEnded) ac.abort(); });

  let upstream;
  try {
    upstream = await fetchT(target, { method: "POST", headers: headersFor(kind, req.headers), body: raw, signal: ac.signal }, 300_000);
  } catch {
    res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ type: "error", error: { type: "api_error", message: `upstream ${kind} unreachable` } }));
    return;
  }

  const ct = upstream.headers.get("content-type") || "application/json";
  const fwd = { "content-type": ct };
  for (const name of ["retry-after", "x-should-retry", "anthropic-ratelimit-unified-requests-limit", "anthropic-ratelimit-unified-requests-remaining", "anthropic-ratelimit-unified-requests-reset", "anthropic-ratelimit-unified-tokens-limit", "anthropic-ratelimit-unified-tokens-remaining", "anthropic-ratelimit-unified-tokens-reset"]) {
    const v = upstream.headers.get(name);
    if (v) fwd[name] = v;
  }
  res.writeHead(upstream.status, fwd);

  if (!upstream.body) return res.end();
  try {
    const reader = upstream.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (res.destroyed) {
        // Cycle-3 #1: client gone mid-body — cancel the upstream stream so
        // the generation stops instead of running to completion (quota burn).
        reader.cancel().catch(() => {});
        break;
      }
      res.write(Buffer.from(value));
    }
  } catch {
    // Cliente abortou (o signal propagado via fetchT derruba o read) ou o
    // upstream caiu no meio do SSE: nada mais a dizer.
  } finally {
    res.end();
  }
});

server.listen(PORT, HOST, () => {
  console.log(`claude-multirouter on http://${HOST}:${PORT} -> codex:${CODEX_UP} nim:${NIM_UP} anthropic:${ANTHROPIC_UP}`);
  // Pre-aquece o cache do catalogo: o discovery do cliente tem timeout de 3s
  // e a primeira chamada fria custa segundos de fan-out.
  modelsPayload().catch(() => {});
});
