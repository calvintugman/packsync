// PackSync hosted storage: a small Cloudflare Worker in front of one R2 bucket.
//
// * Accounts authenticate with Microsoft (Minecraft) device-code sign-in (`POST /v1/auth/microsoft`);
//   the server verifies the Microsoft RS256 ID token, hashes `(tid, sub)` with a domain separator,
//   and wraps/unwraps the account's 32-byte encryption key with AES-256-GCM under `VAULT_SECRET`.
// * Signing in on another computer with the same Microsoft account issues a new per-device session
//   and unwraps the existing vault key so all devices share one encrypted namespace (`u/<account>/`).
// * Modpacks, configs, and worlds are encrypted locally with XChaCha20-Poly1305 before upload;
//   objects support create-only and compare-and-swap writes (`If-None-Match: *`, `If-Match`),
//   ranged reads, and non-recursive listings.
// * Optional SPAKE2 pairing relays short-lived messages through `/v1/pairings`.
//
// API (all JSON errors are `{"error": code, "message": text}`; answers always have a body, because an
// empty one can arrive marked as compressed):
//   GET    /                                  service info
//   GET    /v1                                service info
//   GET    /privacy                           Privacy Policy & Terms of Use (HTML)
//   GET    /terms                             Privacy Policy & Terms of Use (HTML)
//   GET    /v1/auth/config                    Microsoft device-code OAuth parameters & policy URLs
//   POST   /v1/auth/microsoft                 verify Microsoft ID token, issue session & vault key
//   POST   /v1/accounts                       anonymous account creation (only when ALLOW_ANONYMOUS_ACCOUNTS=true)
//   DELETE /v1/accounts                       delete the account, its sessions, usage rows, and all R2 objects
//   GET    /v1/usage                          {used, quota}
//   GET    /v1/objects?prefix=p/&cursor=c     {objects:[{key,size,etag,uploaded_ms}], prefixes, cursor}
//   HEAD   /v1/objects/<key>                  metadata (ETag, Content-Length, X-Uploaded-Ms)
//   GET    /v1/objects/<key>                  body; `Range: bytes=a-b` gives 206
//   PUT    /v1/objects/<key>                  If-None-Match: * | If-Match: "<etag>"; X-Content-SHA256
//   DELETE /v1/objects/<key>
//   POST   /v1/pairings                       {nameplate, expires_ms}
//   PUT    /v1/pairings/<n>/<slot>            slots a, c: the account owner; b, d: the joining app
//   GET    /v1/pairings/<n>/<slot>            200 body | 202 not written yet | 404 gone
//   DELETE /v1/pairings/<n>

import { PRIVACY_PAGE } from "./privacy-page";

export interface Env {
  BUCKET: R2Bucket;
  DB: D1Database;
  ACCOUNT_QUOTA_BYTES: string;
  TOTAL_QUOTA_BYTES: string;
  REGISTRATIONS_PER_DAY: string;
  GLOBAL_REGISTRATIONS_PER_DAY?: string;
  MAX_OBJECT_BYTES: string;
  MAX_OBJECTS_PER_ACCOUNT?: string;
  MAX_WRITES_PER_DAY?: string;
  MAX_READS_PER_DAY?: string;
  INACTIVE_DAYS?: string;
  ALLOW_ANONYMOUS_ACCOUNTS?: string;
  MICROSOFT_CLIENT_ID?: string;
  VAULT_SECRET?: string;
}

const PAIRING_TTL_MS = 10 * 60 * 1000;
const PAIRING_SLOT_MAX = 4096;
const KEY_MAX = 512;
const DAY_MS = 86_400_000;
const HOUR_MS = 3_600_000;
const MAX_SESSIONS_PER_ACCOUNT = 10;
const CLOCK_SKEW_SECS = 60;
const MS_JWKS_URL = "https://login.microsoftonline.com/common/discovery/v2.0/keys";

const SECURITY_HEADERS: Record<string, string> = {
  "strict-transport-security": "max-age=31536000; includeSubDomains",
  "x-content-type-options": "nosniff",
  "x-frame-options": "DENY",
};

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly headers: Record<string, string> = {},
  ) {
    super(message);
  }
}

function withSecurityHeaders(res: Response): Response {
  const headers = new Headers(res.headers);
  for (const [k, v] of Object.entries(SECURITY_HEADERS)) {
    headers.set(k, v);
  }
  return new Response(res.body, {
    status: res.status,
    statusText: res.statusText,
    headers,
  });
}

function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

function fail(status: number, code: string, message: string, headers: Record<string, string> = {}): never {
  throw new HttpError(status, code, message, headers);
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

function b64urlDecode(s: string): Uint8Array<ArrayBuffer> {
  if (!/^[A-Za-z0-9_-]*$/.test(s) || s.length % 4 === 1) {
    throw new Error("invalid base64url");
  }
  const padded = s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4);
  const bin = atob(padded);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) {
    out[i] = bin.charCodeAt(i);
  }
  return out;
}

async function sha256Hex(text: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return Array.from(new Uint8Array(d), (x) => x.toString(16).padStart(2, "0")).join("");
}

/** Collapse IPv6 clients to their /64 prefix so host-part rotation cannot bypass daily limits. */
function clientSubnet(req: Request): string {
  const raw = (req.headers.get("cf-connecting-ip") ?? "unknown").trim().toLowerCase().split("%")[0] ?? "";
  if (!raw.includes(":")) return raw || "unknown";
  const [head = "", tail] = raw.split("::");
  const hParts = head ? head.split(":").filter(Boolean) : [];
  const tParts = tail !== undefined ? tail.split(":").filter(Boolean) : [];
  const fill = Math.max(0, 8 - hParts.length - tParts.length);
  const full = [...hParts, ...Array<string>(fill).fill("0"), ...tParts];
  if (full.length !== 8) return raw;
  return `${full.slice(0, 4).map((g) => g.padStart(4, "0")).join(":")}::/64`;
}

async function takeRateSlot(env: Env, bucket: string, limit: number): Promise<boolean> {
  const day = Math.floor(Date.now() / DAY_MS);
  const counted = await env.DB.prepare(
    `INSERT INTO registrations (ip_hash, day, n) VALUES (?1, ?2, 1)
     ON CONFLICT (ip_hash, day) DO UPDATE SET n = n + 1 WHERE n < ?3`,
  )
    .bind(bucket, day, limit)
    .run();
  if (Math.random() < 0.05) {
    await env.DB.prepare("DELETE FROM registrations WHERE day < ?1").bind(day - 2).run();
  }
  return counted.meta.changes > 0;
}

async function enforceRegistrationLimits(req: Request, env: Env): Promise<void> {
  const subnet = clientSubnet(req);
  const ipHash = await sha256Hex(`packsync-registration:${subnet}`);
  const perSubnetLimit = num(env.REGISTRATIONS_PER_DAY, 2);
  if (!(await takeRateSlot(env, ipHash, perSubnetLimit))) {
    fail(429, "too_many_accounts", "too many new accounts from this network today; try again tomorrow", {
      "retry-after": "3600",
    });
  }
  const globalKey = await sha256Hex("packsync-global-registration");
  const globalLimit = num(env.GLOBAL_REGISTRATIONS_PER_DAY, 40);
  if (!(await takeRateSlot(env, globalKey, globalLimit))) {
    fail(429, "too_many_accounts", "daily account registration limit reached; try again tomorrow", {
      "retry-after": "3600",
    });
  }
}

async function chargeOp(env: Env, accountId: string, op: "write" | "read"): Promise<void> {
  const day = Math.floor(Date.now() / DAY_MS);
  const limit = op === "write" ? num(env.MAX_WRITES_PER_DAY, 3000) : num(env.MAX_READS_PER_DAY, 25000);
  const col = op === "write" ? "writes" : "reads";
  const r = await env.DB.prepare(
    `INSERT INTO op_usage (account, day, writes, reads) VALUES (?1, ?2, ?3, ?4)
     ON CONFLICT (account, day) DO UPDATE SET ${col} = ${col} + 1 WHERE ${col} < ?5`,
  )
    .bind(accountId, day, op === "write" ? 1 : 0, op === "read" ? 1 : 0, limit)
    .run();
  if (r.meta.changes === 0) {
    if (op === "write") {
      fail(429, "daily_write_limit", "daily write operation limit reached; try again tomorrow", {
        "retry-after": "3600",
      });
    }
    fail(429, "daily_read_limit", "daily read operation limit reached; try again tomorrow", {
      "retry-after": "3600",
    });
  }
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
  const acctId = m[1];
  const tokenHash = await sha256Hex(m[2]);
  const row = await env.DB.prepare("SELECT id, token_hash, used, quota, last_seen_ms FROM accounts WHERE id = ?1")
    .bind(acctId)
    .first<{ id: string; token_hash: string; used: number; quota: number; last_seen_ms: number }>();
  if (!row) fail(401, "unauthorized", "unknown account or wrong token");

  let matchedSession = false;
  let sessionLastSeen = 0;
  if (!sameHex(row.token_hash, tokenHash)) {
    const sess = await env.DB.prepare(
      "SELECT token_hash, last_seen_ms FROM sessions WHERE account = ?1 AND token_hash = ?2",
    )
      .bind(acctId, tokenHash)
      .first<{ token_hash: string; last_seen_ms: number }>();
    if (!sess || !sameHex(sess.token_hash, tokenHash)) {
      fail(401, "unauthorized", "unknown account or wrong token");
    }
    matchedSession = true;
    sessionLastSeen = sess.last_seen_ms;
  }

  const now = Date.now();
  const updates: D1PreparedStatement[] = [];
  if (now - row.last_seen_ms > DAY_MS) {
    updates.push(env.DB.prepare("UPDATE accounts SET last_seen_ms = ?2 WHERE id = ?1").bind(row.id, now));
  }
  if (matchedSession && now - sessionLastSeen > DAY_MS) {
    updates.push(
      env.DB.prepare("UPDATE sessions SET last_seen_ms = ?3 WHERE account = ?1 AND token_hash = ?2").bind(
        row.id,
        tokenHash,
        now,
      ),
    );
  }
  if (updates.length === 1) await updates[0].run();
  else if (updates.length > 1) await env.DB.batch(updates);

  return { id: row.id, used: row.used, quota: row.quota };
}

// ------------------------------------------------------------------------------- Microsoft auth

interface JwkEntry {
  kty?: unknown;
  use?: unknown;
  alg?: unknown;
  kid?: unknown;
  n?: unknown;
  e?: unknown;
}

interface JwtHeader {
  alg?: unknown;
  kid?: unknown;
}

interface MsIdClaims {
  aud?: unknown;
  iss?: unknown;
  tid?: unknown;
  sub?: unknown;
  exp?: unknown;
  nbf?: unknown;
  name?: unknown;
  preferred_username?: unknown;
}

let jwksCache: { keys: Map<string, CryptoKey>; fetchedAt: number } | null = null;

function requireMicrosoftClientId(env: Env): string {
  const clientId = env.MICROSOFT_CLIENT_ID?.trim() ?? "";
  if (!clientId) {
    fail(503, "auth_not_configured", "Microsoft sign-in is not configured on this service");
  }
  return clientId;
}

function authConfig(req: Request, env: Env): Response {
  const clientId = requireMicrosoftClientId(env);
  const origin = new URL(req.url).origin;
  return json({
    microsoft_client_id: clientId,
    device_code_url: "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode",
    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    scope: "openid profile offline_access",
    privacy_url: `${origin}/privacy`,
    terms_url: `${origin}/terms`,
  });
}

async function fetchMsJwks(): Promise<Map<string, CryptoKey>> {
  const resp = await fetch(MS_JWKS_URL, { headers: { accept: "application/json" } });
  if (!resp.ok) {
    fail(503, "ms_keys_unavailable", "could not fetch Microsoft signing keys");
  }
  const data = (await resp.json().catch(() => null)) as { keys?: unknown } | null;
  if (!data || !Array.isArray(data.keys)) {
    fail(503, "ms_keys_unavailable", "invalid Microsoft signing keys response");
  }
  const map = new Map<string, CryptoKey>();
  for (const raw of data.keys) {
    if (typeof raw !== "object" || raw === null) continue;
    const jwk = raw as JwkEntry;
    if (jwk.kty !== "RSA" || typeof jwk.kid !== "string" || !jwk.kid) continue;
    if (typeof jwk.n !== "string" || typeof jwk.e !== "string") continue;
    if (typeof jwk.use === "string" && jwk.use !== "sig") continue;
    if (typeof jwk.alg === "string" && jwk.alg !== "RS256") continue;
    try {
      const key = await crypto.subtle.importKey(
        "jwk",
        { kty: "RSA", n: jwk.n, e: jwk.e },
        { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
        false,
        ["verify"],
      );
      map.set(jwk.kid, key);
    } catch {
      // Ignore keys with unsupported JWK parameters rather than rejecting the key set.
    }
  }
  jwksCache = { keys: map, fetchedAt: Date.now() };
  return map;
}

async function getMsSigningKey(kid: string): Promise<CryptoKey | null> {
  if (jwksCache && Date.now() - jwksCache.fetchedAt < HOUR_MS) {
    const hit = jwksCache.keys.get(kid);
    if (hit) return hit;
  }
  const fresh = await fetchMsJwks();
  return fresh.get(kid) ?? null;
}

function parseJwtJson<T>(part: string): T {
  const bytes = b64urlDecode(part);
  const text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes);
  const parsed: unknown = JSON.parse(text);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error("jwt section must be a JSON object");
  }
  return parsed as T;
}

async function verifyMicrosoftIdToken(
  idToken: string,
  clientId: string,
): Promise<{ tid: string; sub: string; displayName: string | null }> {
  const parts = idToken.split(".");
  if (parts.length !== 3 || parts.some((p) => !p)) {
    fail(401, "invalid_token", "malformed Microsoft ID token");
  }
  let header: JwtHeader;
  let claims: MsIdClaims;
  let sig: Uint8Array<ArrayBuffer>;
  try {
    header = parseJwtJson<JwtHeader>(parts[0]);
    claims = parseJwtJson<MsIdClaims>(parts[1]);
    sig = b64urlDecode(parts[2]);
  } catch {
    fail(401, "invalid_token", "malformed Microsoft ID token");
  }

  if (header.alg !== "RS256" || typeof header.kid !== "string" || !header.kid || header.kid.length > 256) {
    fail(401, "invalid_token", "unsupported Microsoft ID token header");
  }
  if (claims.aud !== clientId) {
    fail(401, "invalid_token", "Microsoft ID token audience mismatch");
  }
  const issMatch =
    typeof claims.iss === "string"
      ? /^https:\/\/login\.microsoftonline\.com\/([0-9a-f-]{36})\/v2\.0$/.exec(claims.iss)
      : null;
  if (!issMatch) {
    fail(401, "invalid_token", "Microsoft ID token issuer is invalid");
  }
  const issGuid = issMatch[1];
  if (typeof claims.tid !== "string" || claims.tid.toLowerCase() !== issGuid) {
    fail(401, "invalid_token", "Microsoft ID token tenant mismatch");
  }
  if (typeof claims.sub !== "string" || claims.sub.length === 0 || claims.sub.length > 256) {
    fail(401, "invalid_token", "Microsoft ID token subject is missing");
  }

  const nowSec = Math.floor(Date.now() / 1000);
  if (typeof claims.exp !== "number" || !Number.isFinite(claims.exp) || nowSec >= claims.exp + CLOCK_SKEW_SECS) {
    fail(401, "invalid_token", "Microsoft ID token has expired");
  }
  if (typeof claims.nbf !== "number" || !Number.isFinite(claims.nbf) || nowSec + CLOCK_SKEW_SECS < claims.nbf) {
    fail(401, "invalid_token", "Microsoft ID token is not valid yet");
  }

  const key = await getMsSigningKey(header.kid);
  if (!key) {
    fail(401, "invalid_token", "unknown Microsoft signing key");
  }
  const signedBytes = new TextEncoder().encode(`${parts[0]}.${parts[1]}`);
  const verified = await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, sig, signedBytes);
  if (!verified) {
    fail(401, "invalid_token", "Microsoft ID token signature verification failed");
  }

  const rawName =
    typeof claims.name === "string" && claims.name.trim()
      ? claims.name.trim()
      : typeof claims.preferred_username === "string" && claims.preferred_username.trim()
        ? claims.preferred_username.trim()
        : null;
  const displayName = rawName ? rawName.slice(0, 120) : null;

  return { tid: issGuid, sub: claims.sub, displayName };
}

async function deriveVaultKek(vaultSecret: string): Promise<CryptoKey> {
  const keyBytes = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(`packsync-vault-kek-v1:${vaultSecret}`),
  );
  return crypto.subtle.importKey("raw", keyBytes, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

async function wrapMasterKey(vaultSecret: string, accountId: string, masterKey: Uint8Array<ArrayBuffer>): Promise<string> {
  const kek = await deriveVaultKek(vaultSecret);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const aad = new TextEncoder().encode(`packsync-vault-v1:${accountId}`);
  const sealed = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: aad }, kek, masterKey);
  return `v1.${b64url(iv)}.${b64url(new Uint8Array(sealed))}`;
}

async function unwrapMasterKey(vaultSecret: string, accountId: string, wrapped: string): Promise<Uint8Array> {
  const parts = wrapped.split(".");
  if (parts.length !== 3 || parts[0] !== "v1") {
    fail(500, "vault_error", "stored account key has an unsupported format");
  }
  let iv: Uint8Array<ArrayBuffer>;
  let sealed: Uint8Array<ArrayBuffer>;
  try {
    iv = b64urlDecode(parts[1]);
    sealed = b64urlDecode(parts[2]);
  } catch {
    fail(500, "vault_error", "stored account key is malformed");
  }
  if (iv.length !== 12 || sealed.length < 32 + 16) {
    fail(500, "vault_error", "stored account key is malformed");
  }
  const kek = await deriveVaultKek(vaultSecret);
  const aad = new TextEncoder().encode(`packsync-vault-v1:${accountId}`);
  let plain: ArrayBuffer;
  try {
    plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: aad }, kek, sealed);
  } catch {
    fail(500, "vault_decrypt_failed", "could not unwrap stored account key");
  }
  const out = new Uint8Array(plain);
  if (out.length !== 32) {
    fail(500, "vault_error", "unwrapped account key has unexpected length");
  }
  return out;
}

async function authMicrosoft(req: Request, env: Env): Promise<Response> {
  const clientId = requireMicrosoftClientId(env);
  const vaultSecret = env.VAULT_SECRET ?? "";
  if (vaultSecret.trim().length < 32) {
    fail(503, "auth_not_configured", "vault encryption secret is not configured on this service");
  }

  const hour = Math.floor(Date.now() / HOUR_MS);
  const rateKey = await sha256Hex(`packsync-ms-auth:${hour}:${clientSubnet(req)}`);
  if (!(await takeRateSlot(env, rateKey, 20))) {
    fail(429, "too_many_attempts", "too many sign-in attempts from this network; wait an hour and try again", {
      "retry-after": "3600",
    });
  }

  const body = (await req.json().catch(() => null)) as { id_token?: unknown } | null;
  const idToken = typeof body?.id_token === "string" ? body.id_token.trim() : "";
  if (!idToken || idToken.length > 16_384) {
    fail(400, "bad_token", "missing or malformed id_token");
  }

  const { tid, sub, displayName } = await verifyMicrosoftIdToken(idToken, clientId);
  const msHash = await sha256Hex(`packsync-ms-v1:${tid}:${sub}`);

  const existing = await env.DB.prepare("SELECT id, wrapped_key, quota FROM accounts WHERE ms_hash = ?1")
    .bind(msHash)
    .first<{ id: string; wrapped_key: string | null; quota: number }>();

  const token = b64url(crypto.getRandomValues(new Uint8Array(32)));
  const tokenHash = await sha256Hex(token);
  const now = Date.now();

  if (existing) {
    if (!existing.wrapped_key) {
      fail(500, "vault_error", "account is missing a wrapped encryption key");
    }
    const masterKey = await unwrapMasterKey(vaultSecret, existing.id, existing.wrapped_key);
    await env.DB.batch([
      env.DB.prepare("UPDATE accounts SET last_seen_ms = ?2 WHERE id = ?1").bind(existing.id, now),
      env.DB.prepare(
        "INSERT INTO sessions (account, token_hash, created_ms, last_seen_ms) VALUES (?1, ?2, ?3, ?3)",
      ).bind(existing.id, tokenHash, now),
      env.DB.prepare(
        `DELETE FROM sessions
         WHERE account = ?1
           AND token_hash NOT IN (
             SELECT token_hash FROM sessions
             WHERE account = ?1
             ORDER BY last_seen_ms DESC, created_ms DESC, rowid DESC
             LIMIT ?2
           )`,
      ).bind(existing.id, MAX_SESSIONS_PER_ACCOUNT),
    ]);
    return json(
      {
        account: existing.id,
        token,
        master_key: b64url(masterKey),
        quota: existing.quota,
        display_name: displayName,
        created: false,
      },
      200,
      { "cache-control": "no-store" },
    );
  }

  await enforceRegistrationLimits(req, env);

  const id = randomId(26);
  const masterKey = crypto.getRandomValues(new Uint8Array(32));
  const wrappedKey = await wrapMasterKey(vaultSecret, id, masterKey);
  const quota = num(env.ACCOUNT_QUOTA_BYTES, 5 * 2 ** 30);

  try {
    await env.DB.batch([
      env.DB.prepare(
        "INSERT INTO accounts (id, token_hash, ms_hash, wrapped_key, created_ms, last_seen_ms, used, quota) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 0, ?6)",
      ).bind(id, "ms-session", msHash, wrappedKey, now, quota),
      env.DB.prepare(
        "INSERT INTO sessions (account, token_hash, created_ms, last_seen_ms) VALUES (?1, ?2, ?3, ?3)",
      ).bind(id, tokenHash, now),
      env.DB.prepare("INSERT INTO counted (account) VALUES (?1)").bind(id),
    ]);
  } catch (e) {
    if (/unique/i.test(String(e))) {
      fail(409, "conflict", "account creation already in progress; retry sign-in");
    }
    throw e;
  }

  return json(
    {
      account: id,
      token,
      master_key: b64url(masterKey),
      quota,
      display_name: displayName,
      created: true,
    },
    201,
    { "cache-control": "no-store" },
  );
}

// ------------------------------------------------------------------------------------- accounts

async function createAccount(req: Request, env: Env): Promise<Response> {
  if (env.ALLOW_ANONYMOUS_ACCOUNTS !== "true") {
    fail(
      403,
      "microsoft_signin_required",
      "anonymous account creation is disabled; sign in with your Minecraft (Microsoft) account",
    );
  }
  await enforceRegistrationLimits(req, env);

  const id = randomId(26);
  const token = b64url(crypto.getRandomValues(new Uint8Array(32)));
  const quota = num(env.ACCOUNT_QUOTA_BYTES, 5 * 2 ** 30);
  const now = Date.now();
  await env.DB.batch([
    env.DB.prepare(
      "INSERT INTO accounts (id, token_hash, created_ms, last_seen_ms, used, quota) VALUES (?1, ?2, ?3, ?3, 0, ?4)",
    ).bind(id, await sha256Hex(token), now, quota),
    env.DB.prepare("INSERT INTO counted (account) VALUES (?1)").bind(id),
  ]);
  return json({ account: id, token, quota }, 201, { "cache-control": "no-store" });
}

async function deleteAccountById(acctId: string, env: Env): Promise<void> {
  // Revoke the account and its auth credentials in D1 first so tokens stop working immediately.
  await env.DB.batch([
    env.DB.prepare("DELETE FROM accounts WHERE id = ?1").bind(acctId),
    env.DB.prepare("DELETE FROM sessions WHERE account = ?1").bind(acctId),
    env.DB.prepare("DELETE FROM op_usage WHERE account = ?1").bind(acctId),
    env.DB.prepare("DELETE FROM pairings WHERE account = ?1").bind(acctId),
    env.DB.prepare("DELETE FROM objects WHERE account = ?1").bind(acctId),
    env.DB.prepare("DELETE FROM counted WHERE account = ?1").bind(acctId),
  ]);
  const prefix = `u/${acctId}/`;
  let cursor: string | undefined;
  do {
    const page = await env.BUCKET.list({ prefix, cursor, limit: 1000 });
    if (page.objects.length > 0) await env.BUCKET.delete(page.objects.map((o) => o.key));
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  COUNTED.delete(acctId);
}

async function deleteAccount(acct: Account, env: Env): Promise<Response> {
  await deleteAccountById(acct.id, env);
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
  await chargeOp(env, acct.id, "write");
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
  if (!Number.isSafeInteger(a) || !Number.isSafeInteger(b) || b < a) {
    fail(416, "bad_range", "invalid range");
  }
  return { offset: a, length: b - a + 1 };
}

async function getObject(req: Request, key: string, acct: Account, env: Env, headOnly: boolean): Promise<Response> {
  const full = `u/${acct.id}/${key}`;
  if (headOnly) {
    await chargeOp(env, acct.id, "read");
    const o = await env.BUCKET.head(full);
    if (!o) return new Response(null, { status: 404 });
    return new Response(null, { status: 200, headers: { ...objectHeaders(o), "content-length": String(o.size) } });
  }
  const range = parseRange(req.headers.get("range"));
  await chargeOp(env, acct.id, "read");
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

/** Atomically reserve `reserved` bytes against account & service quotas before R2 upload. */
async function reserveSpace(env: Env, acct: Account, reserved: number): Promise<void> {
  if (reserved <= 0) return;
  const total = await env.DB.prepare("SELECT COALESCE(SUM(used), 0) AS t FROM accounts").first<{ t: number }>();
  if ((total?.t ?? 0) + reserved > num(env.TOTAL_QUOTA_BYTES, 200 * 2 ** 30)) {
    fail(507, "service_full", "the PackSync service is full; ask whoever runs it for more space");
  }
  const r = await env.DB.prepare("UPDATE accounts SET used = used + ?2 WHERE id = ?1 AND used + ?2 <= quota")
    .bind(acct.id, reserved)
    .run();
  if (r.meta.changes === 0) {
    const cur = await env.DB.prepare("SELECT used, quota FROM accounts WHERE id = ?1")
      .bind(acct.id)
      .first<{ used: number; quota: number }>();
    fail(507, "quota", `this account's space is full (${cur?.used ?? acct.used} of ${cur?.quota ?? acct.quota} bytes used)`);
  }
}

async function releaseReserved(env: Env, acctId: string, reserved: number): Promise<void> {
  if (reserved <= 0) return;
  await env.DB.prepare("UPDATE accounts SET used = MAX(0, used - ?2) WHERE id = ?1").bind(acctId, reserved).run();
}

/** `key` now holds `size` bytes (`null`: deleted), adjusting `used` minus any prior `reserved`. */
async function record(env: Env, acct: Account, key: string, size: number | null, reserved = 0): Promise<void> {
  const old = "COALESCE((SELECT size FROM objects WHERE account = ?1 AND key = ?2), 0)";
  await env.DB.batch([
    env.DB.prepare(`UPDATE accounts SET used = MAX(0, used - ${old} + ?3 - ?4) WHERE id = ?1`).bind(
      acct.id,
      key,
      size ?? 0,
      reserved,
    ),
    size === null
      ? env.DB.prepare("DELETE FROM objects WHERE account = ?1 AND key = ?2").bind(acct.id, key)
      : env.DB.prepare(
          "INSERT INTO objects (account, key, size) VALUES (?1, ?2, ?3) ON CONFLICT (account, key) DO UPDATE SET size = excluded.size",
        ).bind(acct.id, key, size),
  ]);
}

/** Accounts known to be fully in `objects` (per Worker instance; the table is the truth). */
const COUNTED = new Set<string>();

/** Accounts from before `objects` existed: list the bucket once and recount `used` from it. */
async function ensureCounted(env: Env, acct: Account): Promise<void> {
  if (COUNTED.has(acct.id)) return;
  if (await env.DB.prepare("SELECT 1 AS x FROM counted WHERE account = ?1").bind(acct.id).first()) {
    COUNTED.add(acct.id);
    return;
  }
  const root = `u/${acct.id}/`;
  let cursor: string | undefined;
  do {
    const page = await env.BUCKET.list({ prefix: root, cursor, limit: 1000 });
    const rows = page.objects.map((o) => [o.key.slice(root.length), o.size] as const);
    const stmts: D1PreparedStatement[] = [];
    // D1 binds at most 100 values per statement
    for (let i = 0; i < rows.length; i += 30) {
      const chunk = rows.slice(i, i + 30);
      const values = chunk.map((_, j) => `(?1, ?${2 + 2 * j}, ?${3 + 2 * j})`).join(", ");
      // OR IGNORE: a row written meanwhile by `record` is newer than this listing
      stmts.push(env.DB.prepare(`INSERT OR IGNORE INTO objects (account, key, size) VALUES ${values}`).bind(acct.id, ...chunk.flat()));
    }
    if (stmts.length > 0) await env.DB.batch(stmts);
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  await env.DB.batch([
    env.DB.prepare(
      "UPDATE accounts SET used = (SELECT COALESCE(SUM(size), 0) FROM objects WHERE account = ?1) WHERE id = ?1",
    ).bind(acct.id),
    env.DB.prepare("INSERT OR IGNORE INTO counted (account) VALUES (?1)").bind(acct.id),
  ]);
  const row = await env.DB.prepare("SELECT used FROM accounts WHERE id = ?1").bind(acct.id).first<{ used: number }>();
  acct.used = row?.used ?? acct.used;
  COUNTED.add(acct.id);
}

async function putObject(req: Request, key: string, acct: Account, env: Env): Promise<Response> {
  const full = `u/${acct.id}/${key}`;
  const len = Number(req.headers.get("content-length") ?? "NaN");
  if (!Number.isInteger(len) || len < 0) fail(411, "length_required", "Content-Length is required");
  if (len > num(env.MAX_OBJECT_BYTES, 90_000_000)) fail(413, "too_large", `objects are limited to ${env.MAX_OBJECT_BYTES} bytes`);
  const sha = req.headers.get("x-content-sha256") ?? "";
  if (!/^[0-9a-f]{64}$/.test(sha)) fail(400, "bad_digest", "X-Content-SHA256 (hex) is required");

  await chargeOp(env, acct.id, "write");

  const ifNone = req.headers.get("if-none-match");
  const ifMatch = req.headers.get("if-match");
  const existing = await env.BUCKET.head(full);
  if (ifNone === "*" && existing) {
    return json({ error: "precondition_failed", message: `${key} already exists`, etag: existing.etag }, 412);
  }
  if (ifMatch !== null && (!existing || existing.httpEtag !== ifMatch.trim())) {
    return json({ error: "precondition_failed", message: `${key} changed meanwhile` }, 412);
  }

  await ensureCounted(env, acct);
  const prevRow = await env.DB.prepare("SELECT size FROM objects WHERE account = ?1 AND key = ?2")
    .bind(acct.id, key)
    .first<{ size: number }>();
  if (!prevRow) {
    const maxObjects = num(env.MAX_OBJECTS_PER_ACCOUNT, 10_000);
    const countRow = await env.DB.prepare("SELECT COUNT(*) AS n FROM objects WHERE account = ?1")
      .bind(acct.id)
      .first<{ n: number }>();
    if ((countRow?.n ?? 0) >= maxObjects) {
      fail(507, "object_limit", `this account is limited to ${maxObjects} objects`);
    }
  }

  const reserved = Math.max(0, len - (prevRow?.size ?? existing?.size ?? 0));
  await reserveSpace(env, acct, reserved);
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
    await releaseReserved(env, acct.id, reserved);
    const msg = String(e);
    if (/digest|checksum|sha/i.test(msg)) fail(400, "bad_digest", "the upload was damaged in transit");
    throw e;
  }
  if (!stored) {
    await releaseReserved(env, acct.id, reserved);
    return json({ error: "precondition_failed", message: `${key} changed meanwhile` }, 412);
  }
  await record(env, acct, key, stored.size, reserved);
  return json({ etag: stored.etag, size: stored.size, uploaded_ms: stored.uploaded.getTime() }, 200, objectHeaders(stored));
}

async function deleteObject(key: string, acct: Account, env: Env): Promise<Response> {
  const full = `u/${acct.id}/${key}`;
  await chargeOp(env, acct.id, "write");
  await ensureCounted(env, acct);
  await env.BUCKET.delete(full);
  await record(env, acct, key, null);
  return json({ ok: true });
}

// ------------------------------------------------------------------------------------- pairing

async function createPairing(acct: Account, env: Env): Promise<Response> {
  const now = Date.now();
  await env.DB.prepare("DELETE FROM pairings WHERE created_ms < ?1").bind(now - PAIRING_TTL_MS).run();
  const active = await env.DB.prepare("SELECT COUNT(*) AS n FROM pairings WHERE account = ?1")
    .bind(acct.id)
    .first<{ n: number }>();
  if ((active?.n ?? 0) >= 10) {
    fail(429, "too_many_pairings", "too many active pairing codes for this account; wait a few minutes");
  }
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
  } else if (slot === "b") {
    const hour = Math.floor(Date.now() / HOUR_MS);
    const key = await sha256Hex(`packsync-pair-join:${hour}:${clientSubnet(req)}`);
    if (!(await takeRateSlot(env, key, 20))) {
      fail(429, "too_many_attempts", "too many pairing attempts from this network; wait an hour and try again");
    }
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

// -------------------------------------------------------------------------------------- cleanup

async function runScheduledCleanup(env: Env): Promise<void> {
  const now = Date.now();
  const today = Math.floor(now / DAY_MS);
  await env.DB.batch([
    env.DB.prepare("DELETE FROM pairings WHERE created_ms < ?1").bind(now - PAIRING_TTL_MS),
    env.DB.prepare("DELETE FROM registrations WHERE day < ?1").bind(today - 2),
    env.DB.prepare("DELETE FROM op_usage WHERE day < ?1").bind(today - 2),
  ]);

  const emptyCutoff = now - 7 * DAY_MS;
  const inactiveDays = num(env.INACTIVE_DAYS, 180);
  const inactiveCutoff = now - inactiveDays * DAY_MS;
  const expired = await env.DB.prepare(
    `SELECT id FROM accounts
     WHERE (used = 0 AND last_seen_ms < ?1) OR last_seen_ms < ?2
     ORDER BY last_seen_ms ASC
     LIMIT 25`,
  )
    .bind(emptyCutoff, inactiveCutoff)
    .all<{ id: string }>();

  for (const row of expired.results) {
    await deleteAccountById(row.id, env);
  }
}

// -------------------------------------------------------------------------------------- routing

async function route(req: Request, env: Env): Promise<Response> {
  const url = new URL(req.url);
  const path = url.pathname;
  const m = req.method;

  if ((path === "/" || path === "/v1") && m === "GET") {
    return json({ service: "packsync", api: 1 });
  }
  if ((path === "/privacy" || path === "/terms") && m === "GET") {
    return new Response(PRIVACY_PAGE, {
      headers: {
        "content-type": "text/html; charset=utf-8",
        "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        "referrer-policy": "no-referrer",
      },
    });
  }
  if (path === "/v1/auth/config" && m === "GET") {
    return authConfig(req, env);
  }
  if (path === "/v1/auth/microsoft" && m === "POST") {
    return authMicrosoft(req, env);
  }
  if (path === "/v1/accounts") {
    if (m === "POST") return createAccount(req, env);
    if (m === "DELETE") return deleteAccount(await authenticate(req, env), env);
  }
  if (path === "/v1/usage" && m === "GET") {
    const a = await authenticate(req, env);
    await ensureCounted(env, a);
    return json({ used: a.used, quota: a.quota });
  }
  if (path === "/v1/objects" && m === "GET") {
    return listObjects(url, await authenticate(req, env), env);
  }
  if (path.startsWith("/v1/objects/")) {
    let key: string;
    try {
      key = decodeURIComponent(path.slice("/v1/objects/".length));
    } catch {
      fail(400, "bad_key", "invalid object key encoding");
    }
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

export async function scheduled(_ctrl: ScheduledController, env: Env, _ctx: ExecutionContext): Promise<void> {
  await runScheduledCleanup(env);
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    try {
      return withSecurityHeaders(await route(req, env));
    } catch (e) {
      if (e instanceof HttpError) {
        return withSecurityHeaders(json({ error: e.code, message: e.message }, e.status, e.headers));
      }
      console.error(e);
      return withSecurityHeaders(json({ error: "internal", message: "internal error" }, 500));
    }
  },
  scheduled,
} satisfies ExportedHandler<Env>;
