#!/bin/sh
# Build the workspace for Windows on Linux and run its test programs under Wine.
#
# This is a check, not a substitute for real Windows: Wine's file system and process table differ from
# NTFS and Windows in details. It finds what cross-platform code gets wrong early (it found two real bugs:
# `\\?\` paths with `/` in them, and a missing windows-sys feature).
#
# Needs: a Rust toolchain with the x86_64-pc-windows-gnu target, zig and cargo-zigbuild (they provide
# the Windows C compiler and linker), and wine. Nothing is installed by this script. One way, all inside
# one folder (TOOLS), without touching the system toolchain:
#   RUSTUP_HOME=$TOOLS/rustup CARGO_HOME=$TOOLS/cargo sh rustup-init.sh -y --no-modify-path \
#       --profile minimal --target x86_64-pc-windows-gnu
#   (zig from https://ziglang.org/download/ in $TOOLS/zig, then: cargo install cargo-zigbuild)
#
# On Windows the tests fake Prism with the `packsync-fake-prism` program instead of shell scripts; the CLI
# hooks suite (it runs the user's own shell scripts) is Unix-only.
set -eu
cd "$(dirname "$0")/.."
TARGET=x86_64-pc-windows-gnu
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/windows}"
cargo zigbuild --target "$TARGET" --workspace --tests
cargo zigbuild --target "$TARGET" -p packsync-fake-prism -p packsync-cli   # programs the tests start
export WINEDEBUG=-all
deps="$CARGO_TARGET_DIR/$TARGET/debug/deps"
fail=0
# the newest build of each test program (older builds keep their hashed names)
names=$(for f in "$deps"/*.exe; do basename "$f" | sed 's/-[0-9a-f]*\.exe$//'; done | sort -u)
for name in $names; do
  case "$name" in packsync|packsync_desktop|packsync_bench|packsync_e2e|packsync_fake_prism|prism_real|killed) continue ;; esac
  exe=""
  for f in "$deps/$name"-*.exe; do
    if [ -z "$exe" ] || [ "$f" -nt "$exe" ]; then exe=$f; fi
  done
  if out=$(wine "$exe" --test-threads=2 2>&1); then r=ok; else r=FAILED; fail=1; fi
  printf '%-22s %-6s %s\n' "$name" "$r" "$(printf '%s\n' "$out" | grep '^test result' | tail -1)"
  if [ "$r" != ok ]; then printf '%s\n' "$out" | grep -E '^test .* FAILED|panicked' | head -20; fi
done
exit $fail
