// The page the app opens in the system browser to create or use a passkey (WebAuthn with the PRF
// extension). It derives a secret only the passkey can produce and hands it to the PackSync app on this
// computer — never to the service. Served at /passkey; the passkey is bound to the domain serving it.
export const PASSKEY_PAGE = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>PackSync passkey</title>
<style>
  :root { color-scheme: light dark; --bg:#f5f6f8; --card:#fff; --text:#0f1720; --muted:#566170; --accent:#047857; --border:#e2e6eb; }
  @media (prefers-color-scheme: dark) { :root { --bg:#0a0d12; --card:#11151c; --text:#e9edf3; --muted:#9ba5b4; --accent:#34d399; --border:#212835; } }
  body { margin:0; min-height:100vh; display:grid; place-items:center; background:var(--bg); color:var(--text);
         font:15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }
  .card { width:min(420px, calc(100vw - 32px)); background:var(--card); border:1px solid var(--border); border-radius:18px;
          padding:28px; box-shadow:0 24px 60px rgb(0 0 0 / 12%); animation: pop .35s cubic-bezier(.34,1.4,.64,1) both; }
  @keyframes pop { from { opacity:0; transform:translateY(8px) scale(.97) } }
  h1 { font-size:20px; margin:0 0 6px } p { margin:0 0 16px; color:var(--muted) }
  button { width:100%; padding:12px; border:0; border-radius:10px; font:inherit; font-weight:650; cursor:pointer;
           background:linear-gradient(180deg,#059669,#047857); color:#fff; }
  button:disabled { opacity:.6; cursor:default }
  .mark { width:42px; height:42px; border-radius:12px; background:linear-gradient(140deg,#34d399,#047857); margin-bottom:14px }
  #msg.error { color:#b42318 } @media (prefers-color-scheme: dark) { #msg.error { color:#ff8a7e } }
</style>
</head>
<body>
<main class="card">
  <div class="mark" aria-hidden="true"></div>
  <h1 id="title">PackSync</h1>
  <p id="msg">Loading…</p>
  <button id="go" hidden></button>
</main>
<script>
(() => {
  const q = new URLSearchParams(location.search);
  const mode = q.get("mode"), cb = q.get("cb") || "", state = q.get("state") || "", label = q.get("label") || "this computer";
  const account = q.get("user") || "";
  const $ = (id) => document.getElementById(id);
  const say = (t, err) => { $("msg").textContent = t; $("msg").className = err ? "error" : ""; };
  const b64u = (buf) => btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\\+/g, "-").replace(/\\//g, "_").replace(/=+$/, "");
  // only ever report to the PackSync app on this computer
  let target;
  try { target = new URL(cb); } catch { target = null; }
  const local = target && target.protocol === "http:" && (target.hostname === "127.0.0.1" || target.hostname === "localhost") && /^[A-Za-z0-9_-]{16,}$/.test(state);
  if (!local || (mode !== "create" && mode !== "get") || (mode === "create" && !/^[a-z2-7]{26}$/.test(account))) {
    $("title").textContent = "This link is not valid";
    say("Open passkeys from the PackSync app.", true);
    return;
  }
  if (!window.PublicKeyCredential) { say("This browser does not support passkeys.", true); return; }
  $("title").textContent = mode === "create" ? "Protect your PackSync cloud" : "Sign in to your PackSync cloud";
  say(mode === "create"
    ? "Create a passkey (fingerprint, face, PIN or your phone). If you ever lose every computer, it gets you back in."
    : "Use the passkey you created for PackSync. Your phone works too.");
  const btn = $("go");
  btn.textContent = mode === "create" ? "Create passkey" : "Use passkey";
  btn.hidden = false;

  async function report(body) {
    const r = await fetch(target.href, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ state, ...body }) });
    if (!r.ok) throw new Error("the PackSync app did not accept the answer (" + r.status + ")");
  }

  btn.onclick = async () => {
    btn.disabled = true;
    say("Waiting for your passkey…");
    try {
      const salt = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode("packsync passkey recovery v1")));
      const challenge = crypto.getRandomValues(new Uint8Array(32));
      const prf = { eval: { first: salt } };
      let rawId, secret, userHandle = null;
      if (mode === "create") {
        const cred = await navigator.credentials.create({ publicKey: {
          rp: { name: "PackSync" },
          user: { id: new TextEncoder().encode(account), name: "PackSync (" + label + ")", displayName: "PackSync" },
          challenge,
          pubKeyCredParams: [{ type: "public-key", alg: -7 }, { type: "public-key", alg: -257 }],
          authenticatorSelection: { residentKey: "required", userVerification: "required" },
          extensions: { prf },
        } });
        rawId = cred.rawId;
        const ext = cred.getClientExtensionResults().prf;
        if (!ext || ext.enabled === false) throw Object.assign(new Error("unsupported"), { unsupported: true });
        secret = ext.results && ext.results.first;
        if (!secret) {
          // most passkeys produce the secret only when used, not when created
          say("Almost done: confirm the passkey once more.");
          const a = await navigator.credentials.get({ publicKey: {
            challenge: crypto.getRandomValues(new Uint8Array(32)),
            allowCredentials: [{ type: "public-key", id: rawId }],
            userVerification: "required",
            extensions: { prf },
          } });
          secret = a.getClientExtensionResults().prf?.results?.first;
        }
      } else {
        const a = await navigator.credentials.get({ publicKey: { challenge, userVerification: "required", extensions: { prf } } });
        rawId = a.rawId;
        userHandle = a.response.userHandle ? new TextDecoder().decode(a.response.userHandle) : null;
        secret = a.getClientExtensionResults().prf?.results?.first;
      }
      if (!secret) throw Object.assign(new Error("unsupported"), { unsupported: true });
      await report({ credential_id: b64u(rawId), prf: b64u(secret), account: userHandle });
      $("title").textContent = "Done";
      say("You can close this tab and return to PackSync.");
      btn.hidden = true;
    } catch (e) {
      btn.disabled = false;
      if (e && e.unsupported) {
        try { await report({ error: "unsupported" }); } catch {}
        say("This passkey can't protect encrypted data (it lacks the PRF feature). Try another passkey manager, or use a recovery key in PackSync.", true);
      } else if (e && e.name === "NotAllowedError") {
        say("Cancelled or timed out. Try again.", true);
      } else {
        say("Something went wrong: " + (e && e.message ? e.message : e), true);
      }
    }
  };
})();
</script>
</body>
</html>`;
