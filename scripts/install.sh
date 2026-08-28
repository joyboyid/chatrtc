#!/usr/bin/env bash
# Install bandit-x from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/joyboyid/bandit-X/main/scripts/install.sh | bash
#   ./scripts/install.sh
#   ./scripts/install.sh --version v0.1.0
#
# Env:
#   BANDIT_X_REPO   default: joyboyid/bandit-X
#   INSTALL_DIR       default: ~/.local/bin

set -euo pipefail

REPO="${BANDIT_X_REPO:-joyboyid/bandit-X}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="latest"

detect_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            case "$arch" in
                x86_64)  echo "x86_64-unknown-linux-gnu" ;;
                aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) echo "unsupported Linux arch: $arch" >&2; exit 1 ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64)  echo "x86_64-apple-darwin" ;;
                arm64)   echo "aarch64-apple-darwin" ;;
                *) echo "unsupported macOS arch: $arch" >&2; exit 1 ;;
            esac
            ;;
        *)
            echo "unsupported OS: $os (use install.ps1 on Windows)" >&2
            exit 1
            ;;
    esac
}

resolve_version() {
    if [[ "$VERSION" == "latest" || "$VERSION" == "--version" ]]; then
        curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep -o '"tag_name": *"[^"]*"' | head -1 | cut -d'"' -f4
    else
        echo "${VERSION#v}"
    fi
}

main() {
    if [[ "${1:-}" == "--version" && -n "${2:-}" ]]; then
        VERSION="$2"
    elif [[ -n "${1:-}" && "${1:-}" != "--version" ]]; then
        VERSION="$1"
    fi

    local target tag asset url tmpdir
    target="$(detect_target)"
    tag="v$(resolve_version)"
    asset="bandit-x-${tag#v}-${target}.tar.gz"

    echo "→ Installing bandit-x ${tag} for ${target}"

    url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    curl -fsSL "$url" -o "${tmpdir}/${asset}"
    tar -xzf "${tmpdir}/${asset}" -C "$tmpdir"

    mkdir -p "$INSTALL_DIR"
    install -m 755 "${tmpdir}/bandit-x-${tag#v}-${target}/bandit-x" "${INSTALL_DIR}/bandit-x"

    echo "✓ Installed to ${INSTALL_DIR}/bandit-x"
    echo ""
    echo "Pastikan ${INSTALL_DIR} ada di PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    echo ""
    echo "Jalankan:"
    echo "  bandit-x --server"
    echo "  bandit-x --client -a SERVER:8080 -u username"
}

main "$@"