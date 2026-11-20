#!/usr/bin/env bash
# Deploy the PackSync service to your Cloudflare account (run `npx wrangler login` once first).
# Safe to run again: existing resources are reused, and schema migrations are idempotent.
set -euo pipefail
cd "$(dirname "$0")"

[ -d node_modules ] || npm install --no-audit --no-fund

echo "== Cloudflare account"
npx wrangler whoami

echo "== R2 bucket packsync-hosted"
if npx wrangler r2 bucket list 2>/dev/null | grep -q "packsync-hosted"; then
  echo "exists"
else
  npx wrangler r2 bucket create packsync-hosted
fi

echo "== D1 database packsync-hosted"
current_id=$(sed -nE 's/^[[:space:]]*database_id[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' wrangler.toml | head -1 || true)
d1_list=$(npx wrangler d1 list --json 2>/dev/null || echo '[]')
id=$(printf '%s' "$d1_list" | CURRENT_ID="$current_id" node -e '
  let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
    const list = JSON.parse(s || "[]");
    const cur = process.env.CURRENT_ID;
    if (cur && cur !== "00000000-0000-0000-0000-000000000000" && list.some(d => d.uuid === cur)) {
      console.log(cur);
      return;
    }
    const db = list.find(d => d.name === "packsync-hosted");
    console.log(db ? db.uuid : "");
  });')
if [ -z "$id" ]; then
  npx wrangler d1 create packsync-hosted
  id=$(npx wrangler d1 list --json | node -e '
    let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
      console.log(JSON.parse(s).find(d => d.name === "packsync-hosted").uuid);
    });')
fi
if [ "$id" != "$current_id" ]; then
  sed -i.bak -E "s|^([[:space:]]*database_id[[:space:]]*=[[:space:]]*\")[^\"]*(\".*)|\1$id\2|" wrangler.toml && rm -f wrangler.toml.bak
  echo "configured database $id in wrangler.toml"
else
  echo "using database $id"
fi

npx wrangler d1 execute packsync-hosted --remote --yes --file=schema.sql

run_d1_alter() {
  local sql="$1"
  local out=""
  if out=$(npx wrangler d1 execute packsync-hosted --remote --yes --command="$sql" 2>&1); then
    return 0
  fi
  if printf '%s' "$out" | grep -qi "duplicate column"; then
    return 0
  fi
  printf '%s\n' "$out" >&2
  return 1
}

echo "== Migrating D1 schema (idempotent)"
run_d1_alter "ALTER TABLE accounts ADD COLUMN ms_hash TEXT;"
run_d1_alter "ALTER TABLE accounts ADD COLUMN wrapped_key TEXT;"
npx wrangler d1 execute packsync-hosted --remote --yes --command="CREATE UNIQUE INDEX IF NOT EXISTS accounts_ms_hash ON accounts(ms_hash);"

echo "== Checking MICROSOFT_CLIENT_ID in wrangler.toml"
ms_client_id=$(sed -nE 's/^[[:space:]]*MICROSOFT_CLIENT_ID[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' wrangler.toml | head -1 || true)
if [ -z "$ms_client_id" ]; then
  cat <<'EOF'
MICROSOFT_CLIENT_ID is currently empty in wrangler.toml.
To enable "Sign in with Minecraft" (Microsoft device-code OAuth):
  1. Open https://entra.microsoft.com/ -> App registrations -> New registration
  2. Set Supported account types to:
     "Accounts in any organizational directory and personal Microsoft accounts"
  3. Under Authentication -> Advanced settings -> Allow public client flows: Yes
  4. Copy the Application (client) ID (UUID)
EOF
  if [ -t 0 ]; then
    read -r -p "Paste MICROSOFT_CLIENT_ID (or press Enter to skip for now): " input_client_id || true
    input_client_id=$(printf '%s' "${input_client_id:-}" | tr -d '[:space:]')
    if [ -n "$input_client_id" ]; then
      if printf '%s' "$input_client_id" | grep -qE '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'; then
        sed -i.bak -E "s|^([[:space:]]*MICROSOFT_CLIENT_ID[[:space:]]*=[[:space:]]*\")[^\"]*(\".*)|\1$input_client_id\2|" wrangler.toml && rm -f wrangler.toml.bak
        echo "configured MICROSOFT_CLIENT_ID in wrangler.toml"
      else
        echo "warning: '$input_client_id' is not a valid UUID; leaving MICROSOFT_CLIENT_ID empty in wrangler.toml" >&2
      fi
    else
      echo "warning: skipping MICROSOFT_CLIENT_ID; /v1/auth/config and /v1/auth/microsoft will return 503 until configured" >&2
    fi
  else
    echo "warning: non-interactive shell; set MICROSOFT_CLIENT_ID in wrangler.toml and re-run ./setup.sh" >&2
  fi
else
  echo "MICROSOFT_CLIENT_ID is configured"
fi

ensure_vault_secret() {
  local secrets_json
  if ! secrets_json=$(npx wrangler secret list --format json 2>/dev/null); then
    return 1
  fi
  if printf '%s' "$secrets_json" | node -e '
    let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
      try {
        const list = JSON.parse(s || "[]");
        process.exit(Array.isArray(list) && list.some(x => x && x.name === "VAULT_SECRET") ? 0 : 1);
      } catch {
        process.exit(1);
      }
    });
  '; then
    echo "VAULT_SECRET already configured"
    return 0
  fi
  local generated
  generated=$(node -e 'process.stdout.write(require("crypto").randomBytes(32).toString("base64url"))')
  printf '%s' "$generated" | npx wrangler secret put VAULT_SECRET
  echo "generated and uploaded VAULT_SECRET"
}

echo "== Checking VAULT_SECRET"
vault_secret_ready=0
if ensure_vault_secret; then
  vault_secret_ready=1
fi

echo "== Deploying the Worker"
npx wrangler deploy | tee .deploy.log
url=$(grep -oE 'https://[a-zA-Z0-9.-]+\.workers\.dev' .deploy.log | head -1 || true)
rm -f .deploy.log

if [ "$vault_secret_ready" -eq 0 ]; then
  echo "== Setting VAULT_SECRET on newly deployed Worker"
  ensure_vault_secret
fi

echo
if [ -n "$url" ]; then
  echo "Deployed: $url"
  curl -fsS "$url/" && echo
  echo "Build PackSync with this address built in:  PACKSYNC_HOSTED_URL=$url cargo build --release -p packsync-cli"
else
  echo "Deployed. Find the address with: npx wrangler deployments list"
fi
