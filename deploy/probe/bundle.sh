#!/bin/bash
# Pack what the Linux probe needs (workspace manifest + proto + net + probe)
# into target/probe-bundle.tar.gz. On the Linux machine:
#   mkdir -p ~/syncra && tar xzf probe-bundle.tar.gz -C ~/syncra && cd ~/syncra
#   cargo build --release -p probe
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
out="$root/target/probe-bundle"
rm -rf "$out"
mkdir -p "$out/crates" "$out/.cargo"
cp "$root/Cargo.toml" "$root/Cargo.lock" "$out/"
cp "$root/.cargo/config.toml" "$out/.cargo/"
cp -r "$root/crates/proto" "$root/crates/net" "$root/crates/probe" "$out/crates/"
tar czf "$root/target/probe-bundle.tar.gz" -C "$out" .
echo "$root/target/probe-bundle.tar.gz"
