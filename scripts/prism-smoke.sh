#!/bin/sh
# Smoke test against the REAL Prism Launcher (Linux only).
#
# * Uses a disposable data directory under target/prism-smoke — never your real Prism data.
# * Copies only the read-only caches (meta/, libraries/, assets/, java/, metacache) from an existing
#   Prism data directory (default: ~/.local/share/PrismLauncher; override with PACKSYNC_PRISM_SEED).
#   It never reads accounts.json, *.cfg or your instances. Reflinks are used when the file system
#   supports them (btrfs, xfs), otherwise this copies a few GB.
# * Runs Prism inside an empty network namespace (nothing leaves the machine) with a local file
#   server for the cached Minecraft metadata, an offline launch, and a wrapper command standing in
#   for Java, so Minecraft itself never starts.
#
# Needs: prismlauncher, unshare (util-linux), ip (iproute2), python3, cargo, and a seed data directory
# in which one instance of Minecraft 1.20.1 (or 1.21.1) has been launched at least once.
set -eu
cd "$(dirname "$0")/.."
for t in prismlauncher unshare ip python3 timeout cargo; do
  command -v "$t" >/dev/null 2>&1 || { echo "missing prerequisite: $t" >&2; exit 2; }
done
unshare --user --map-root-user --net true 2>/dev/null || { echo "user namespaces are not available here (unshare --user --net failed)" >&2; exit 2; }
exec cargo test -p packsync-cli --test prism_real -- --ignored --nocapture --test-threads=1 "$@"
