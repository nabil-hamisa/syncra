#!/usr/bin/env bash
# Build Syncra-setup-<version>.exe: a portable Syncra.exe (AVX2, so it
# runs on any Intel since Haswell and any Ryzen; static C runtime, so no
# VC++ redistributable) plus the ViGEmBus gamepad driver, packed by Inno Setup.
#
# The dev build uses target-cpu=native and must not be copied to other PCs;
# this one goes to target/portable and leaves it alone.
#
# Usage: deploy/windows/build-setup.sh   → target/setup/Syncra-setup-<version>.exe
#   VIGEM=path/to/ViGEmBus_1.22.0.exe   (required; github.com/nefarius/ViGEmBus/releases)
#   ISCC=path/to/ISCC.exe               (default: Inno Setup 6 in Program Files)
#   SYNCRA_ENV=path/to/env.sh           (optional; sourced first to set up cargo)
#   SYNCRA_SIGNAL=host:7000             (optional; default signal server baked in)
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
if [ -n "${SYNCRA_ENV:-}" ]; then source "$SYNCRA_ENV"; fi
ISCC="${ISCC:-/c/Program Files (x86)/Inno Setup 6/ISCC.exe}"
VIGEM="${VIGEM:-}"
[ -x "$ISCC" ] || { echo "Inno Setup not found at $ISCC (set ISCC)"; exit 1; }
[ -f "$VIGEM" ] || { echo "ViGEmBus setup not found at $VIGEM (set VIGEM)"; exit 1; }

cd "$root"
version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static" \
    cargo build --release -p app --target-dir target/portable

exe="target/portable/release/syncra.exe"
mkdir -p target/setup
# MSYS would rewrite the /D... switches as paths.
MSYS2_ARG_CONV_EXCL="*" "$ISCC" /Q \
    "/DAppVersion=$version" \
    "/DAppExe=$(cygpath -w "$root/$exe")" \
    "/DVigem=$(cygpath -w "$VIGEM")" \
    "/O$(cygpath -w "$root/target/setup")" \
    "$(cygpath -w "$root/deploy/windows/syncra.iss")"
echo "built target/setup/Syncra-setup-$version.exe"
