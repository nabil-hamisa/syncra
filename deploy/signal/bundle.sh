#!/bin/bash
# Pack only what the signal server needs into a self-contained build
# directory (Docker build context), then tar it for copying to the server.
#   deploy/signal/bundle.sh            -> target/signal-bundle.tar.gz
#   scp target/signal-bundle.tar.gz user@server:
#   ssh user@server 'mkdir -p syncra-signal && tar xzf signal-bundle.tar.gz -C syncra-signal && cd syncra-signal && docker compose up -d --build'
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
out="$root/target/signal-bundle"
rm -rf "$out"
mkdir -p "$out/crates"
cp "$root/Cargo.toml" "$root/Cargo.lock" "$out/"
cp -r "$root/crates/proto" "$root/crates/signal" "$out/crates/"
cp "$root/deploy/signal/Dockerfile" "$root/deploy/signal/docker-compose.yml" "$out/"
tar czf "$root/target/signal-bundle.tar.gz" -C "$out" .
echo "$root/target/signal-bundle.tar.gz"
