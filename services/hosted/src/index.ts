// PackSync hosted storage: a small Cloudflare Worker in front of one R2 bucket.
//
// * Anyone can create an account (no e-mail, no password): the app gets an account ID and a random
//   token. Each account has its own key space (`u/<account>/...`) and a space limit; all accounts
//   together have one too, so the operator's bill is bounded.
// * The app encrypts everything before upload; this service never sees a key and cannot read data.
// * Objects support what PackSync's storage contract needs: create-only and compare-and-swap writes
//   (`If-None-Match: *`, `If-Match`), ranged reads, non-recursive listings.
// * Pairing a second computer uses a short-lived mailbox: the two apps run a password-authenticated
//   key exchange (SPAKE2) through it, so the service only relays messages it cannot use.
//
// API (all JSON errors are `{"error": code, "message": text}`; answers always have a body, because an
// empty one can arrive marked as compressed):
//   POST   /v1/accounts                       create an account (rate limited per network address)
//   GET    /v1/usage                          {used, quota}
//   DELETE /v1/accounts                       delete the account and all its objects
//   GET    /v1/objects?prefix=p/&cursor=c     {objects:[{key,size,etag,uploaded_ms}], prefixes, cursor}
//   HEAD   /v1/objects/<key>                  metadata (ETag, Content-Length, X-Uploaded-Ms)
//   GET    /v1/objects/<key>                  body; `Range: bytes=a-b` gives 206
//   PUT    /v1/objects/<key>                  If-None-Match: * | If-Match: "<etag>"; X-Content-SHA256
//   DELETE /v1/objects/<key>
//   POST   /v1/pairings                       {nameplate, expires_ms}
//   PUT    /v1/pairings/<n>/<slot>            slots a, c: the account owner; b, d: the joining app
//   GET    /v1/pairings/<n>/<slot>            200 body | 202 not written yet | 404 gone
//   DELETE /v1/pairings/<n>

export interface Env {
  BUCKET: R2Bucket;
  DB: D1Database;
  ACCOUNT_QUOTA_BYTES: string;
  TOTAL_QUOTA_BYTES: string;
  REGISTRATIONS_PER_DAY: string;
  MAX_OBJECT_BYTES: string;
}

const PAIRING_TTL_MS = 10 * 60 * 1000;
const PAIRING_SLOT_MAX = 4096;
const KEY_MAX = 512;

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

function fail(status: number, code: string, message: string): never {
  throw new HttpError(status, code, message);
}

function num(v: string | undefined, fallback: number): number {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

function randomId(bytes: number, alphabet = "abcdefghijklmnopqrstuvwxyz234567"): string {
  const b = crypto.getRandomValues(new Uint8Array(bytes));
  return Array.from(b, (x) => alphabet[x % alphabet.length]).join("");
}

function b64url(bytes: Uint8Array): string {
  let s = "";
  for (const x of bytes) s += String.fromCharCode(x);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

async function sha256Hex(text: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return Array.from(new Uint8Array(d), (x) => x.toString(16).padStart(2, "0")).join("");
}

/** Constant-time comparison of two equal-length hex strings. */
function sameHex(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/** Keys as PackSync writes them: `/`-separated, letters, digits, `.`, `_`, `-`; no empty or dot segments. */
function validKey(key: string, dir: boolean): boolean {
  if (key.length === 0 || key.length > KEY_MAX) return false;
  if (dir !== key.endsWith("/")) return false;
  const parts = (dir ? key.slice(0, -1) : key).split("/");
  return parts.every((p) => p.length > 0 && p !== "." && p !== ".." && /^[A-Za-z0-9._-]+$/.test(p));
}

interface Account {
  id: string;
  used: number;
  quota: number;
}

async function authenticate(req: Request, env: Env): Promise<Account> {
  const h = req.headers.get("authorization") ?? "";
  const m = /^Bearer ([a-z2-7]{26})\.([A-Za-z0-9_-]{43})$/.exec(h);
  if (!m) fail(401, "unauthorized", "missing or malformed credentials");
  const row = await env.DB.prepare("SELECT id, token_hash, used, quota, last_seen_ms FROM accounts WHERE id = ?1")
    .bind(m[1])
    .first<{ id: string; token_hash: string; used: number; quota: number; last_seen_ms: number }>();
  if (!row || !sameHex(row.token_hash, await sha256Hex(m[2]))) {
    fail(401, "unauthorized", "unknown account or wrong token");
  }
  const now = Date.now();
  if (now - row.last_seen_ms > 86_400_000) {
    await env.DB.prepare("UPDATE accounts SET last_seen_ms = ?2 WHERE id = ?1").bind(row.id, now).run();
  }
  return { id: row.id, used: row.used, quota: row.quota };
}

// ------------------------------------------------------------------------------------- accounts

async function createAccount(req: Request, env: Env): Promise<Response> {
  const ip = req.headers.get("cf-connecting-ip") ?? "unknown";
  const ipHash = await sha256Hex(`packsync-registration:${ip}`);
  const day = Math.floor(Date.now() / 86_400_000);
  const limit = num(env.REGISTRATIONS_PER_DAY, 5);
  const counted = await env.DB.prepare(
    `INSERT INTO registrations (ip_hash, day, n) VALUES (?1, ?2, 1)
     ON CONFLICT (ip_hash, day) DO UPDATE SET n = n + 1 WHERE n < ?3`,
  )
    .bind(ipHash, day, limit)
    .run();
  if (counted.meta.changes === 0) {
    fail(429, "too_many_accounts", "too many new accounts from this network today; try again tomorrow");
  }
  await env.DB.prepare("DELETE FROM registrations WHERE day < ?1").bind(day - 2).run();

  const id = randomId(26);
  const token = b64url(crypto.getRandomValues(new Uint8Array(32)));
  const quota = num(env.ACCOUNT_QUOTA_BYTES, 20 * 2 ** 30);
  const now = Date.now();
  await env.DB.prepare(
    "INSERT INTO accounts (id, token_hash, created_ms, last_seen_ms, used, quota) VALUES (?1, ?2, ?3, ?3, 0, ?4)",
  )
    .bind(id, await sha256Hex(token), now, quota)
    .run();
  return json({ account: id, token, quota }, 201);
}

async function deleteAccount(acct: Account, env: Env): Promise<Response> {
  const prefix = `u/${acct.id}/`;
  let cursor: string | undefined;
  do {
    const page = await env.BUCKET.list({ prefix, cursor, limit: 1000 });
    if (page.objects.length > 0) await env.BUCKET.delete(page.objects.map((o) => o.key));
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  await env.DB.batch([
    env.DB.prepare("DELETE FROM accounts WHERE id = ?1").bind(acct.id),
    env.DB.prepare("DELETE FROM pairings WHERE account = ?1").bind(acct.id),
  ]);
  return json({ ok: true });
}

// -------------------------------------------------------------------------------------- objects

function objectHeaders(o: R2Object): Record<string, string> {
  return {
    etag: o.httpEtag,
    "x-uploaded-ms": String(o.uploaded.getTime()),
    "x-object-size": String(o.size),
    "content-type": "application/octet-stream",
  };
}

async function listObjects(url: URL, acct: Account, env: Env): Promise<Response> {
  const prefix = url.searchParams.get("prefix") ?? "";
  if (prefix !== "" && !validKey(prefix, true)) fail(400, "bad_key", "the prefix must be a directory key ending in /");
  const root = `u/${acct.id}/`;
  const page = await env.BUCKET.list({
    prefix: root + prefix,
    delimiter: "/",
    cursor: url.searchParams.get("cursor") ?? undefined,
    limit: 1000,
  });
  return json({
    objects: page.objects.map((o) => ({
      key: o.key.slice(root.length),
      size: o.size,
      etag: o.etag,
      uploaded_ms: o.uploaded.getTime(),
    })),
    prefixes: page.delimitedPrefixes.map((p) => p.slice(root.length)),
    cursor: page.truncated ? page.cursor : null,
  });
}

/** `Range: bytes=a-b` (the only form the app sends). */
function parseRange(h: string | null): { offset: number; length: number } | null {
  if (!h) return null;
  const m = /^bytes=(\d+)-(\d+)$/.exec(h.trim());
  if (!m) fail(400, "bad_range", "only `bytes=a-b` ranges are supported");
  const a = Number(m[1]);
  const b = Number(m[2]);
  if (b < a) fail(416, "bad_range", "empty range");
  return { offset: a, length: b - a + 1 };
}

async function getObject(req: Request, key: string, acct: Account, env: Env, headOnly: boolean): Promise<Response> {
  const full = `u/${acct.id}/${key}`;
  if (headOnly) {
    const o = await env.BUCKET.head(full);
    if (!o) return new Response(null, { status: 404 });
    return new Response(null, { status: 200, headers: { ...objectHeaders(o), "content-length": String(o.size) } });
  }
  const range = parseRange(req.headers.get("range"));
  if (range) {
    const meta = await env.BUCKET.head(full);
    if (!meta) fail(404, "not_found", `${key} does not exist`);
    if (range.offset >= meta.size) fail(416, "bad_range", "the range starts after the end of the object");
    range.length = Math.min(range.length, meta.size - range.offset);
    const o = await env.BUCKET.get(full, { range });
    if (!o) fail(404, "not_found", `${key} does not exist`);
    return new Response(o.body, {
      status: 206,
      headers: {
        ...objectHeaders(o),
        "content-length": String(range.length),
        "content-range": `bytes ${range.offset}-${range.offset + range.length - 1}/${o.size}`,
      },
    });
  }
  const o = await env.BUCKET.get(full);
  if (!o) fail(404, "not_found", `${key} does not exist`);
  return new Response(o.body, { status: 200, headers: { ...objectHeaders(o), "content-length": String(o.size) } });
}

/** Reserve `delta` bytes of the account's (and the service's) space; negative releases. */
async function reserve(env: Env, acct: Account, delta: number): Promise<void> {
  if (delta <= 0) {
    if (delta < 0) {
      await env.DB.prepare("UPDATE accounts SET used = MAX(0, used + ?2) WHERE id = ?1").bind(acct.id, delta).run();
    }
    return;
  }
  const total = await env.DB.prepare("SELECT COALESCE(SUM(used), 0) AS t FROM accounts").first<{ t: number }>();
  if ((total?.t ?? 0) + delta > num(env.TOTAL_QUOTA_BYTES, 200 * 2 ** 30)) {
    fail(507, "service_full", "the PackSync service is full; ask whoever runs it for more space");
  }
  const r = await env.DB.prepare("UPDATE accounts SET used = used + ?2 WHERE id = ?1 AND used + ?2 <= quota")
    .bind(acct.id, delta)
    .run();
  if (r.meta.changes === 0) {
    fail(507, "quota", `this account's space is full (${acct.used} of ${acct.quota} bytes used)`);
  }
}

async function putObject(req: Request, key: string, acct: Account, env: Env): Promise<Response> {
  const full = `u/${acct.id}/${key}`;
  const len = Number(req.headers.get("content-length") ?? "NaN");
  if (!Number.isInteger(len) || len < 0) fail(411, "length_required", "Content-Length is required");
  if (len > num(env.MAX_OBJECT_BYTES, 90_000_000)) fail(413, "too_large", `objects are limited to ${env.MAX_OBJECT_BYTES} bytes`);
  const sha = req.headers.get("x-content-sha256") ?? "";
  if (!/^[0-9a-f]{64}$/.test(sha)) fail(400, "bad_digest", "X-Content-SHA256 (hex) is required");

  const ifNone = req.headers.get("if-none-match");
  const ifMatch = req.headers.get("if-match");
  const existing = await env.BUCKET.head(full);
  if (ifNone === "*" && existing) {
    return json({ error: "precondition_failed", message: `${key} already exists`, etag: existing.etag }, 412);
  }
  if (ifMatch !== null && (!existing || existing.httpEtag !== ifMatch.trim())) {
    return json({ error: "precondition_failed", message: `${key} changed meanwhile` }, 412);
  }

  const delta = len - (existing?.size ?? 0);
  await reserve(env, acct, delta);
  const onlyIf = new Headers();
  if (ifNone === "*") onlyIf.set("if-none-match", "*");
  if (ifMatch !== null) onlyIf.set("if-match", ifMatch.trim());
  let stored: R2Object | null = null;
  try {
    const body = req.body ?? new Uint8Array(0);
    stored = await env.BUCKET.put(full, body, {
      sha256: sha,
      onlyIf: ifNone === "*" || ifMatch !== null ? onlyIf : undefined,
      httpMetadata: { contentType: "application/octet-stream" },
    });
  } catch (e) {
    await reserve(env, acct, -delta);
    const msg = String(e);
    if (/digest|checksum|sha/i.test(msg)) fail(400, "bad_digest", "the upload was damaged in transit");
    throw e;
  }
  if (!stored) {
    await reserve(env, acct, -delta);
    return json({ error: "precondition_failed", message: `${key} changed meanwhile` }, 412);
  }
  return json({ etag: stored.etag, size: stored.size, uploaded_ms: stored.uploaded.getTime() }, 200, objectHeaders(stored));
}

async function deleteObject(key: string, acct: Account, env: Env): Promise<Response> {
  const full = `u/${acct.id}/${key}`;
  const existing = await env.BUCKET.head(full);
  if (existing) {
    await env.BUCKET.delete(full);
    await reserve(env, acct, -existing.size);
  }
  return json({ ok: true });
}

// ------------------------------------------------------------------------------------- pairing

async function createPairing(acct: Account, env: Env): Promise<Response> {
  const now = Date.now();
  await env.DB.prepare("DELETE FROM pairings WHERE created_ms < ?1").bind(now - PAIRING_TTL_MS).run();
  for (let i = 0; i < 20; i++) {
    const nameplate = String(1000 + (crypto.getRandomValues(new Uint32Array(1))[0] % 9000));
    const r = await env.DB.prepare(
      "INSERT INTO pairings (nameplate, account, created_ms) VALUES (?1, ?2, ?3) ON CONFLICT (nameplate) DO NOTHING",
    )
      .bind(nameplate, acct.id, now)
      .run();
    if (r.meta.changes === 1) return json({ nameplate, expires_ms: now + PAIRING_TTL_MS }, 201);
  }
  fail(503, "busy", "no pairing code is free right now; try again in a minute");
}

const OWNER_SLOTS = new Set(["a", "c"]);
const JOINER_SLOTS = new Set(["b", "d"]);

async function pairingSlot(req: Request, env: Env, nameplate: string, slot: string): Promise<Response> {
  if (!/^\d{4}$/.test(nameplate) || !(OWNER_SLOTS.has(slot) || JOINER_SLOTS.has(slot))) {
    fail(404, "not_found", "no such pairing");
  }
  const live = Date.now() - PAIRING_TTL_MS;
  const row = await env.DB.prepare(
    "SELECT account, a, b, c, d FROM pairings WHERE nameplate = ?1 AND created_ms > ?2",
  )
    .bind(nameplate, live)
    .first<Record<string, string | null>>();
  if (!row) fail(404, "expired", "this pairing code has expired or was already used");

  if (req.method === "GET") {
    const v = row[slot];
    if (v === null || v === undefined) return json({ pending: true }, 202);
    return new Response(v, { status: 200, headers: { "content-type": "text/plain" } });
  }
  if (req.method !== "PUT") fail(405, "method", "use GET or PUT");
  if (OWNER_SLOTS.has(slot)) {
    const acct = await authenticate(req, env);
    if (acct.id !== row.account) fail(403, "forbidden", "this pairing belongs to another account");
  }
  const body = await req.text();
  if (body.length === 0 || body.length > PAIRING_SLOT_MAX || !/^[A-Za-z0-9_-]+$/.test(body)) {
    fail(400, "bad_message", "pairing messages are short base64url text");
  }
  // first writer wins: a guessed code gets one attempt
  const r = await env.DB.prepare(`UPDATE pairings SET ${slot} = ?2 WHERE nameplate = ?1 AND ${slot} IS NULL`)
    .bind(nameplate, body)
    .run();
  if (r.meta.changes === 0) fail(409, "taken", "this pairing code was already used; make a new one");
  return json({ ok: true }, 201);
}

async function deletePairing(acct: Account, env: Env, nameplate: string): Promise<Response> {
  await env.DB.prepare("DELETE FROM pairings WHERE nameplate = ?1 AND account = ?2").bind(nameplate, acct.id).run();
  return json({ ok: true });
}

// -------------------------------------------------------------------------------------- routing

async function route(req: Request, env: Env): Promise<Response> {
  const url = new URL(req.url);
  const path = url.pathname;
  const m = req.method;

  if (path === "/" || path === "/v1") {
    return json({ service: "packsync", api: 1 });
  }
  if (path === "/v1/accounts") {
    if (m === "POST") return createAccount(req, env);
    if (m === "DELETE") return deleteAccount(await authenticate(req, env), env);
  }
  if (path === "/v1/usage" && m === "GET") {
    const a = await authenticate(req, env);
    return json({ used: a.used, quota: a.quota });
  }
  if (path === "/v1/objects" && m === "GET") {
    return listObjects(url, await authenticate(req, env), env);
  }
  if (path.startsWith("/v1/objects/")) {
    const key = decodeURIComponent(path.slice("/v1/objects/".length));
    if (!validKey(key, false)) fail(400, "bad_key", "invalid object key");
    const acct = await authenticate(req, env);
    if (m === "GET" || m === "HEAD") return getObject(req, key, acct, env, m === "HEAD");
    if (m === "PUT") return putObject(req, key, acct, env);
    if (m === "DELETE") return deleteObject(key, acct, env);
  }
  if (path === "/v1/pairings" && m === "POST") {
    return createPairing(await authenticate(req, env), env);
  }
  const pm = /^\/v1\/pairings\/(\d{4})(?:\/([a-d]))?$/.exec(path);
  if (pm) {
    if (pm[2]) return pairingSlot(req, env, pm[1], pm[2]);
    if (m === "DELETE") return deletePairing(await authenticate(req, env), env, pm[1]);
  }
  fail(404, "not_found", "no such endpoint");
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    try {
      return await route(req, env);
    } catch (e) {
      if (e instanceof HttpError) return json({ error: e.code, message: e.message }, e.status);
      console.error(e);
      return json({ error: "internal", message: "internal error" }, 500);
    }
  },
} satisfies ExportedHandler<Env>;
