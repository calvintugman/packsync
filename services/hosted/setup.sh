#!/usr/bin/env bash
# Deploy the PackSync service to your Cloudflare account (run `npx wrangler login` once first).
# Safe to run again: existing resources are reused, the schema uses IF NOT EXISTS.
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
if grep -q 'database_id = "00000000-0000-0000-0000-000000000000"' wrangler.toml; then
  id=$(npx wrangler d1 list --json 2>/dev/null | node -e '
    let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
      const db = JSON.parse(s).find(d => d.name === "packsync-hosted");
      console.log(db ? db.uuid : "");
    });')
  if [ -z "$id" ]; then
    npx wrangler d1 create packsync-hosted
    id=$(npx wrangler d1 list --json | node -e '
      let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
        console.log(JSON.parse(s).find(d => d.name === "packsync-hosted").uuid);
      });')
  fi
  sed -i.bak "s/database_id = \"00000000-0000-0000-0000-000000000000\"/database_id = \"$id\"/" wrangler.toml && rm -f wrangler.toml.bak
  echo "using database $id"
else
  echo "already configured in wrangler.toml"
fi
npx wrangler d1 execute packsync-hosted --remote --file=schema.sql

echo "== Deploying the Worker"
npx wrangler deploy | tee .deploy.log
url=$(grep -oE 'https://[a-zA-Z0-9.-]+\.workers\.dev' .deploy.log | head -1 || true)
rm -f .deploy.log
echo
if [ -n "$url" ]; then
  echo "Deployed: $url"
  curl -fsS "$url/" && echo
  echo "Build PackSync with this address built in:  PACKSYNC_HOSTED_URL=$url cargo build --release -p packsync-cli"
else
  echo "Deployed. Find the address with: npx wrangler deployments list"
fi
