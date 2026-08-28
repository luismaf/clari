#!/usr/bin/env bash
# One-liner install for clari:
#   curl -fsSL https://raw.githubusercontent.com/luismaf/clari/master/scripts/install.sh | bash
#   pin a release:  ... | bash -s -- -v 0.4.1      (or: bash -s 0.4.1)
#
# Detects your system and picks the right method:
#   Ubuntu/Debian : .deb package via apt
#   Arch          : PKGBUILD via makepkg (the yay way, without needing the AUR)
#   macOS         : release binary into ~/.local/bin
#   Windows       : cargo install --git (needs a Rust toolchain, e.g. rustup)
#   other Linux   : release binary into ~/.local/bin
#
# It never touches your services or config: run 'clari --install' if you
# want the systemd user service (boot autorun).
#
# Env overrides (handy for testing): CLARI_VERSION=v0.4.1 to pin a release,
# CLARI_FORCE=arch|deb|mac|windows|linux to force a branch, CLARI_DRY_RUN=1
# to only print what would happen. CLI: '-v VERSION' or bare VERSION pins a
# release, e.g.  curl -fsSL <url> | bash -s -- -v 0.4.1
set -euo pipefail

usage() {
    cat <<'EOF'
One-liner install for clari:
  curl -fsSL https://raw.githubusercontent.com/luismaf/clari/master/scripts/install.sh | bash

Detects your system and picks the right method:
  Ubuntu/Debian : .deb package via apt
  Arch          : PKGBUILD via makepkg (the yay way, without needing the AUR)
  macOS         : release binary into ~/.local/bin
  Windows       : cargo install --git (needs a Rust toolchain, e.g. rustup)
  other Linux   : release binary into ~/.local/bin

Options:
  -v, --version VERSION   install a specific release instead of latest
  -h, --help              show this help

Env overrides: CLARI_VERSION (pin), CLARI_FORCE (arch|deb|mac|windows|linux),
CLARI_DRY_RUN=1 (print what would happen, touch nothing).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        -v|--version)
            [ $# -ge 2 ] || { echo "option $1 needs a value (e.g. -v 0.4.1)" >&2; exit 1; }
            CLARI_VERSION="$2"
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        -*)
            if [[ "$1" == v* ]] || [[ "$1" == ?*.*.* ]]; then CLARI_VERSION="$1"; shift
            else echo "unknown option: $1 (try --help)" >&2; exit 1; fi
            ;;
        *) CLARI_VERSION="$1"; shift ;;
    esac
done

REPO="luismaf/clari"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

sudo_if() {
    if [ "$(id -u)" = "0" ]; then "$@"; else sudo "$@"; fi
}

VERSION="${CLARI_VERSION:-}"
if [ -z "$VERSION" ]; then
    echo "-> fetching latest release..."
    VERSION="$(
        curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\(v[^"]*\)".*/\1/p' \
            | head -1
    )"
fi
[ -n "$VERSION" ] || { echo "could not determine the latest version (no network?)" >&2; exit 1; }
VERSION="v${VERSION#v}"
echo "-> clari $VERSION"

OS="$(uname -s)"
MACH="$(uname -m)"

detect() {
    case "$OS" in
        MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
        Darwin*) echo "mac" ;;
        Linux*)
            if [ -f /etc/os-release ]; then
                if grep -qi "arch" /etc/os-release; then echo "arch"
                elif grep -qi "debian\|ubuntu" /etc/os-release; then echo "deb"
                else echo "linux"; fi
            else echo "linux"; fi
            ;;
        *) echo "linux" ;;
    esac
}
BRANCH="${CLARI_FORCE:-$(detect)}"

install_binary() { # generic Linux/macOS (ARM): tar.gz into ~/.local/bin
    if [ "$OS" = "Darwin" ]; then
        case "$MACH" in
            arm64|aarch64) TRIPLE="aarch64-apple-darwin" ;;
            *)
                echo "-> Intel Mac detected: no prebuilt binary is published for x86_64-apple-darwin"
                echo "   (GitHub retired the Intel macOS runners). Building from source instead..."
                echo "   Needs a Rust toolchain: https://rustup.rs"
                command -v cargo >/dev/null 2>&1 || { echo "   cargo not found — install Rust first: https://rustup.rs" >&2; exit 1; }
                [ "${CLARI_DRY_RUN:-0}" = "1" ] && { echo "-> (dry-run) would run: cargo install --git https://github.com/$REPO.git --tag $VERSION"; return 0; }
                cargo install --git "https://github.com/$REPO.git" --tag "$VERSION"
                echo "-> done: ~/.cargo/bin/clari ($VERSION)"
                return 0
                ;;
        esac
    else
        case "$MACH" in
            x86_64)        TRIPLE="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
            *) echo "unsupported platform: $OS $MACH" >&2; exit 1 ;;
        esac
    fi
    URL="https://github.com/$REPO/releases/download/$VERSION/clari-$TRIPLE.tar.gz"
    echo "-> downloading clari $VERSION ($TRIPLE)..."
    [ "${CLARI_DRY_RUN:-0}" = "1" ] && { echo "-> (dry-run) would download $URL"; return 0; }
    curl -fsSL "$URL" -o "$TMP/clari.tar.gz"
    tar xzf "$TMP/clari.tar.gz" -C "$TMP"
    mkdir -p "$HOME/.local/bin"
    install -m755 "$TMP/clari" "$HOME/.local/bin/clari"
    echo "-> done: ~/.local/bin/clari ($VERSION)"
}

install_arch() { # the yay way, straight from this repo (no AUR needed)
    echo "-> Arch detected: building the PKGBUILD (yay -S clari, but from source)..."
    git clone --quiet --depth 1 --branch "$VERSION" "https://github.com/$REPO.git" "$TMP/clari"
    cd "$TMP/clari/packaging/aur"
    [ "${CLARI_DRY_RUN:-0}" = "1" ] && { echo "-> (dry-run) would run: makepkg -si"; return 0; }
    makepkg -si
}

install_deb() { # Ubuntu/Debian: .deb via apt (resolves dependencies)
    case "$MACH" in
        x86_64)            DEB="clari_${VERSION#v}_amd64.deb" ;;
        aarch64|arm64)     DEB="clari_${VERSION#v}_arm64.deb" ;;
        *) echo "no .deb for $MACH; falling back to the generic binary"; install_binary; return 0 ;;
    esac
    echo "-> Ubuntu/Debian detected: installing $DEB via apt..."
    curl -fsSL "https://github.com/$REPO/releases/download/$VERSION/$DEB" -o "$TMP/$DEB"
    [ "${CLARI_DRY_RUN:-0}" = "1" ] && { echo "-> (dry-run) would run: sudo apt-get install -y $TMP/$DEB"; return 0; }
    sudo_if apt-get install -y "$TMP/$DEB"
}

install_windows() { # Windows (git-bash): build with Rust
    echo "-> Windows detected: building with cargo (needs a Rust toolchain)."
    echo "   No Rust yet? Get it: https://rustup.rs (then run this script again)."
    if ! command -v cargo >/dev/null 2>&1; then
        echo "   cargo not found — install Rust first: https://rustup.rs" >&2
        exit 1
    fi
    [ "${CLARI_DRY_RUN:-0}" = "1" ] && { echo "-> (dry-run) would run: cargo install --git https://github.com/$REPO.git"; return 0; }
    cargo install --git "https://github.com/$REPO.git" --tag "$VERSION"
}

echo "-> clari $VERSION · branch: $BRANCH"
case "$BRANCH" in
    arch) install_arch ;;
    deb) install_deb ;;
    mac) install_binary ;;
    windows) install_windows ;;
    linux) install_binary ;;
    *) echo "unsupported: $BRANCH" >&2; exit 1 ;;
esac

echo
echo "Try it:        clari -s"
echo "Background:    clari --install   (systemd user service, boot autorun)"
echo "Remove it:     clari --uninstall (service) · rm ~/.local/bin/clari (binary)"
