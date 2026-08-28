#!/usr/bin/env bash
# Build release binaries for common targets (local dev / manual release).
#
# Usage: ./scripts/build-all.sh
#
# Requires:
#   - rustup target add ... (script will try to add missing targets)
#   - Linux ARM64 cross: gcc-aarch64-linux-gnu (sudo apt install gcc-aarch64-linux-gnu)
#   - macOS targets: only works on macOS or with osxcross (skipped otherwise)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/dist"
VERSION="$(grep '^version' "$ROOT/Cargo.toml" | head -1 | cut -d'"' -f2)"

TARGETS=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    x86_64-pc-windows-msvc
)

if [[ "$(uname -s)" == "Darwin" ]]; then
    TARGETS+=(x86_64-apple-darwin aarch64-apple-darwin)
fi

mkdir -p "$OUT"
cd "$ROOT"

for target in "${TARGETS[@]}"; do
    echo "=== Building $target ==="
    rustup target add "$target" 2>/dev/null || true

    extra_env=()
    if [[ "$target" == "aarch64-unknown-linux-gnu" ]]; then
        extra_env=(env CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc)
    fi

    if ! "${extra_env[@]}" cargo build --release --target "$target"; then
        echo "⚠ Skipped $target (missing toolchain or linker)"
        continue
    fi

    pkg="$OUT/bandit-x-${VERSION}-${target}"
    mkdir -p "$pkg"

    if [[ "$target" == *"windows"* ]]; then
        cp "target/$target/release/bandit-x.exe" "$pkg/"
        (cd "$OUT" && zip -r "${pkg}.zip" "$(basename "$pkg")")
    else
        cp "target/$target/release/bandit-x" "$pkg/"
        tar -czf "${pkg}.tar.gz" -C "$OUT" "$(basename "$pkg")"
    fi

    echo "✓ $pkg"
done

echo ""
echo "Done. Packages in: $OUT"