#!/usr/bin/env bash
# One-liner install for clari:
#   curl -fsSL https://raw.githubusercontent.com/luismaf/clari/master/scripts/install.sh | bash
#   pin a release:  ... | bash -s -- -v 0.6.5      (or: bash -s 0.6.5)
#
# Detects your system and picks the right method:
#   Ubuntu/Debian : .deb package via apt
#   Arch          : PKGBUILD via makepkg (the yay way, without needing the AUR)
#   macOS         : release binary into ~/.local/bin
#   Windows       : cargo install --git (needs a Rust toolchain, e.g. rustup)
#   other Linux   : release binary into ~/.local/bin
#
# If there is no published release yet (or the asset for your platform is
# missing), it falls back to building from source with cargo into
# ~/.local/bin instead of failing.
#
# It never touches your services or config: run 'clari' (or 'clari --install')
# afterwards if you want the systemd user service (boot autorun).
#
# Env overrides (handy for testing): CLARI_VERSION=v0.6.5 to pin a release,
# CLARI_FORCE=arch|deb|mac|windows|linux|source to force a branch,
# CLARI_DRY_RUN=1 to only print what would happen, CLARI_REF=<branch|sha>
# to build a specific git ref from source. CLI: '-v VERSION' or bare
# VERSION pins a release, e.g.  curl -fsSL <url> | bash -s -- -v 0.6.5
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
  (no release yet / asset missing: builds from source with cargo)

Options:
  -v, --version VERSION   install a specific release instead of latest
  -s, --source            build from source with cargo (skip the releases)
  -h, --help              show this help

Env overrides: CLARI_VERSION (pin), CLARI_FORCE (arch|deb|mac|windows|linux|source),
CLARI_REF (git ref to build from source; default: latest tag or master),
CLARI_DRY_RUN=1 (print what would happen, touch nothing).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        -v|--version)
            [ $# -ge 2 ] || { echo "option $1 needs a value (e.g. -v 0.6.5)" >&2; exit 1; }
            CLARI_VERSION="$2"
            shift 2
            ;;
        -s|--source) CLARI_FORCE="source"; shift ;;
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
DRY="${CLARI_DRY_RUN:-0}"
INSTALLED_DIR=""

sudo_if() {
    if [ "$(id -u)" = "0" ]; then "$@"; else sudo "$@"; fi
}

# GitHub API, quiet: prints the body or nothing (no network, rate limit...).
gh_api() {
    curl -sSL --max-time 20 -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$REPO/$1" 2>/dev/null || true
}

# Does this URL exist (HTTP 2xx/3xx)? Used before downloading a release asset.
url_ok() {
    [ "$DRY" = "1" ] && return 0
    curl -fsIL --max-time 20 -o /dev/null "$1" 2>/dev/null
}

# ── Which version? ────────────────────────────────────────────────────
# 1. pinned (CLARI_VERSION / -v)  2. latest GitHub release
# 3. newest v* tag (release still building)  4. none → source build
VERSION="${CLARI_VERSION:-}"
RELEASE_OK=0
if [ -n "$VERSION" ]; then
    VERSION="v${VERSION#v}"
    RELEASE_OK=1
else
    echo "-> fetching latest release..."
    VERSION="$(gh_api releases/latest | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\(v[^"]*\)".*/\1/p' | head -1)"
    if [ -n "$VERSION" ]; then
        RELEASE_OK=1
    else
        VERSION="$(gh_api tags | sed -n 's/.*"name"[[:space:]]*:[[:space:]]*"\(v[0-9][^"]*\)".*/\1/p' | head -1)"
        if [ -n "$VERSION" ]; then
            echo "-> no published release yet, but tag $VERSION exists: release assets may still be building."
        fi
    fi
fi

if [ -n "$VERSION" ]; then
    echo "-> clari $VERSION"
else
    echo "-> no release or tag found on GitHub (no network, or nothing published yet)."
    echo "   Falling back to a source build of the master branch."
fi

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
[ -z "$VERSION" ] && [ "$BRANCH" != "windows" ] && BRANCH="source"

# ── Source build (the universal fallback) ─────────────────────────────
need_cargo() {
    command -v cargo >/dev/null 2>&1 && return 0
    echo "   cargo not found — clari needs a Rust toolchain to build from source." >&2
    echo "   Install it with rustup (one line, no root):" >&2
    echo "     curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y" >&2
    echo "   then re-run this installer." >&2
    exit 1
}

install_source() { # cargo install --git into ~/.local/bin (any OS with cargo)
    local ref="${CLARI_REF:-$VERSION}"
    local refarg=()
    if [ -n "$ref" ]; then
        if [[ "$ref" == v[0-9]* ]]; then refarg=(--tag "$ref"); else refarg=(--branch "$ref"); fi
    fi
    echo "-> building clari from source (${ref:-master}) with cargo into ~/.local/bin..."
    need_cargo
    if [ "$DRY" = "1" ]; then
        echo "-> (dry-run) would run: cargo install --git https://github.com/$REPO.git ${refarg[*]:-} --locked --force --root $HOME/.local"
        INSTALLED_DIR="$HOME/.local/bin"
        return 0
    fi
    mkdir -p "$HOME/.local/bin"
    cargo install --git "https://github.com/$REPO.git" "${refarg[@]}" --locked --force --root "$HOME/.local"
    INSTALLED_DIR="$HOME/.local/bin"
    echo "-> done: ~/.local/bin/clari (built from ${ref:-master})"
}

fallback_source() { # called when a release asset is missing
    echo "   $1"
    echo "   Falling back to a source build."
    install_source
}

install_binary() { # generic Linux/macOS (ARM): tar.gz into ~/.local/bin
    if [ "$OS" = "Darwin" ]; then
        case "$MACH" in
            arm64|aarch64) TRIPLE="aarch64-apple-darwin" ;;
            *)
                echo "-> Intel Mac detected: no prebuilt binary is published for x86_64-apple-darwin"
                echo "   (GitHub retired the Intel macOS runners). Building from source instead..."
                install_source
                return 0
                ;;
        esac
    else
        case "$MACH" in
            x86_64)        TRIPLE="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
            *) echo "-> no prebuilt binary for $OS $MACH"; install_source; return 0 ;;
        esac
    fi
    URL="https://github.com/$REPO/releases/download/$VERSION/clari-$TRIPLE.tar.gz"
    if [ "$RELEASE_OK" != "1" ] || ! url_ok "$URL"; then
        fallback_source "release asset not available yet: $URL"
        return 0
    fi
    echo "-> downloading clari $VERSION ($TRIPLE)..."
    [ "$DRY" = "1" ] && { echo "-> (dry-run) would download $URL"; INSTALLED_DIR="$HOME/.local/bin"; return 0; }
    curl -fsSL "$URL" -o "$TMP/clari.tar.gz"
    tar xzf "$TMP/clari.tar.gz" -C "$TMP"
    mkdir -p "$HOME/.local/bin"
    install -m755 "$TMP/clari" "$HOME/.local/bin/clari"
    INSTALLED_DIR="$HOME/.local/bin"
    echo "-> done: ~/.local/bin/clari ($VERSION)"
}

install_arch() { # the yay way, straight from this repo (no AUR needed)
    if ! command -v makepkg >/dev/null 2>&1 || ! command -v git >/dev/null 2>&1; then
        fallback_source "makepkg or git not found."
        return 0
    fi
    if ! url_ok "https://github.com/$REPO/archive/refs/tags/$VERSION.tar.gz"; then
        fallback_source "tag $VERSION has no source tarball on GitHub yet."
        return 0
    fi
    echo "-> Arch detected: building the PKGBUILD (yay -S clari, but from source)..."
    if [ "$DRY" = "1" ]; then
        echo "-> (dry-run) would run: git clone --branch $VERSION https://github.com/$REPO.git && makepkg -si"
        INSTALLED_DIR="/usr/bin"
        return 0
    fi
    git clone --quiet --depth 1 --branch "$VERSION" "https://github.com/$REPO.git" "$TMP/clari"
    cd "$TMP/clari/packaging/aur"
    # The PKGBUILD pins its own pkgver; make it follow the tag we checked out
    # so an older PKGBUILD never builds the wrong tarball.
    sed -i "s/^pkgver=.*/pkgver=${VERSION#v}/" PKGBUILD
    makepkg -si --noconfirm
    cd - >/dev/null
    INSTALLED_DIR="/usr/bin"
}

install_deb() { # Ubuntu/Debian: .deb via apt (resolves dependencies)
    case "$MACH" in
        x86_64)            DEB="clari_${VERSION#v}_amd64.deb" ;;
        aarch64|arm64)     DEB="clari_${VERSION#v}_arm64.deb" ;;
        *) echo "no .deb for $MACH; falling back to the generic binary"; install_binary; return 0 ;;
    esac
    URL="https://github.com/$REPO/releases/download/$VERSION/$DEB"
    if [ "$RELEASE_OK" != "1" ] || ! url_ok "$URL"; then
        fallback_source "release asset not available yet: $URL"
        return 0
    fi
    echo "-> Ubuntu/Debian detected: installing $DEB via apt..."
    [ "$DRY" = "1" ] && { echo "-> (dry-run) would run: sudo apt-get install -y $TMP/$DEB"; INSTALLED_DIR="/usr/bin"; return 0; }
    curl -fsSL "$URL" -o "$TMP/$DEB"
    sudo_if apt-get install -y "$TMP/$DEB"
    INSTALLED_DIR="/usr/bin"
}

install_windows() { # Windows (git-bash): build with Rust
    echo "-> Windows detected: building with cargo (needs a Rust toolchain)."
    echo "   No Rust yet? Get it: https://rustup.rs (then run this script again)."
    need_cargo
    local refarg=()
    [ -n "$VERSION" ] && refarg=(--tag "$VERSION")
    if [ "$DRY" = "1" ]; then
        echo "-> (dry-run) would run: cargo install --git https://github.com/$REPO.git ${refarg[*]:-} --locked --force"
        INSTALLED_DIR="$HOME/.cargo/bin"
        return 0
    fi
    cargo install --git "https://github.com/$REPO.git" "${refarg[@]}" --locked --force
    INSTALLED_DIR="$HOME/.cargo/bin"
}

echo "-> clari ${VERSION:-(source)} · branch: $BRANCH"
case "$BRANCH" in
    arch) install_arch ;;
    deb) install_deb ;;
    mac) install_binary ;;
    windows) install_windows ;;
    linux) install_binary ;;
    source) install_source ;;
    *) echo "unsupported: $BRANCH" >&2; exit 1 ;;
esac

# ═══════════════════════════════════════════════════════════════════════
# POST-INSTALL CHECKS
# ═══════════════════════════════════════════════════════════════════════

echo
echo "── post-install checks ──"

# ── 1. Detect conflicting copies ──────────────────────────────────────
INSTALLED_BIN=""
if [ -n "${INSTALLED_DIR:-}" ]; then
    INSTALLED_BIN="$INSTALLED_DIR/clari"
fi

echo "-> checking PATH for conflicting copies..."
CONFLICT_FOUND=0
WHILE_IFS="$IFS"
IFS=:
for dir in $PATH; do
    candidate="$dir/clari"
    [ -z "$dir" ] && candidate="./clari"
    if [ -x "$candidate" ]; then
        REAL=$(readlink -f "$candidate" 2>/dev/null || echo "$candidate")
        INSTALLED_REAL=""
        if [ -n "$INSTALLED_BIN" ]; then
            INSTALLED_REAL=$(readlink -f "$INSTALLED_BIN" 2>/dev/null || echo "$INSTALLED_BIN")
        fi
        if [ -n "$INSTALLED_REAL" ] && [ "$REAL" = "$INSTALLED_REAL" ]; then
            echo "   ✓ $candidate -> $REAL (this is the one we installed)"
        else
            CONFLICT_VER=$("$candidate" --version 2>/dev/null | head -1 || echo "(unknown version)")
            echo "   ⚠ CONFLICT: $candidate  [$CONFLICT_VER]"
            echo "     This is NOT the copy we just installed (${INSTALLED_BIN:-?})."
            CONFLICT_FOUND=1
            if pacman -Qi clari &>/dev/null 2>&1 && [ "$dir" = "/usr/bin" ]; then
                echo "     Detected: installed via pacman."
                echo "     To fix:   sudo pacman -R clari"
            elif dpkg -l clari 2>/dev/null | grep -q "^ii" && [ "$dir" = "/usr/bin" ]; then
                echo "     Detected: installed via dpkg/apt."
                echo "     To fix:   sudo apt-get remove clari"
            elif rpm -q clari &>/dev/null 2>&1 && [ "$dir" = "/usr/bin" ]; then
                echo "     Detected: installed via rpm."
                echo "     To fix:   sudo rpm -e clari"
            else
                echo "     To fix:   rm $candidate"
            fi
            echo "     The first copy in PATH wins — if $dir is before ${INSTALLED_DIR:-~/.local/bin},"
            echo "     the old copy shadows the new one."
        fi
    fi
done
IFS="$WHILE_IFS"

if [ "$CONFLICT_FOUND" -eq 1 ]; then
    echo
    echo "⚠  A conflicting clari was found. The one in PATH may not be the new one."
    echo "   Fix the conflict above, then re-run this script."
    echo
fi

# ── 2. Check PATH includes install dir ────────────────────────────────
if [ -n "${INSTALLED_DIR:-}" ]; then
    PATH_HAS_DIR=0
    OLD_IFS="$IFS"
    IFS=:
    for dir in $PATH; do
        [ "$dir" = "$INSTALLED_DIR" ] && { PATH_HAS_DIR=1; break; }
    done
    IFS="$OLD_IFS"

    if [ "$PATH_HAS_DIR" -eq 0 ]; then
        echo
        echo "⚠  $INSTALLED_DIR is NOT in your PATH."
        echo "   The binary is there, but your shell can't find it."
        echo
        SHELL_NAME=$(basename "${SHELL:-/bin/bash}")
        RC_FILE=""
        case "$SHELL_NAME" in
            bash) RC_FILE="$HOME/.bashrc" ;;
            zsh)  RC_FILE="$HOME/.zshrc" ;;
            fish)
                RC_FILE="$HOME/.config/fish/config.fish"
                echo "   Add this line to $RC_FILE:"
                echo "     fish_add_path $INSTALLED_DIR"
                ;;
            *) RC_FILE="$HOME/.profile" ;;
        esac
        if [ "$SHELL_NAME" != "fish" ] && [ -n "$RC_FILE" ]; then
            LINE="export PATH=\"\$HOME/.local/bin:\$PATH\""
            if [ "$INSTALLED_DIR" = "$HOME/.cargo/bin" ]; then
                LINE="export PATH=\"\$HOME/.cargo/bin:\$PATH\""
            fi
            echo "   Add this line to $RC_FILE:"
            echo "     $LINE"
            if [ "$DRY" = "1" ]; then
                echo "   (dry-run: not touching $RC_FILE)"
            elif [ -f "$RC_FILE" ] && ! grep -qF "$INSTALLED_DIR" "$RC_FILE" 2>/dev/null; then
                echo "   Adding it now..."
                echo "" >> "$RC_FILE"
                echo "# clari (${VERSION:-source} installed $(date +%Y-%m-%d))" >> "$RC_FILE"
                echo "$LINE" >> "$RC_FILE"
                echo "   ✓ Added to $RC_FILE (restart your shell or run: source $RC_FILE)"
            elif [ -f "$RC_FILE" ]; then
                echo "   (already present in $RC_FILE — skipping)"
            else
                echo "   ($RC_FILE does not exist — create it or add the line manually)"
            fi
        fi
    else
        echo "✓ $INSTALLED_DIR is in PATH"
    fi
fi

# ── 3. Verify binary works and version matches ────────────────────────
echo
echo "-> verifying clari works..."
if [ "$DRY" = "1" ]; then
    echo "   (dry-run: nothing was installed, skipping)"
else
    CLARI_BIN=$(command -v clari 2>/dev/null || echo "")
    [ -z "$CLARI_BIN" ] && [ -n "$INSTALLED_BIN" ] && [ -x "$INSTALLED_BIN" ] && CLARI_BIN="$INSTALLED_BIN"
    if [ -z "$CLARI_BIN" ]; then
        echo "⚠  'clari' not found in PATH after install."
        echo "   The binary should be at ${INSTALLED_BIN:-${INSTALLED_DIR:-~/.local/bin}/clari}"
        echo "   You may need to restart your shell or fix PATH (see above)."
    else
        if ! "$CLARI_BIN" -s >/dev/null 2>&1; then
            echo "⚠  $CLARI_BIN exists but 'clari -s' failed."
            echo "   Check dependencies or run with RUST_LOG=debug."
        else
            echo "✓ 'clari -s' works"
        fi
        INSTALLED_VER=$("$CLARI_BIN" --version 2>/dev/null | head -1 || echo "")
        if [ -n "$VERSION" ] && [ -n "$INSTALLED_VER" ]; then
            EXPECTED_VER="clari ${VERSION#v}"
            INSTALLED_CLEAN=$(echo "$INSTALLED_VER" | sed 's/-.*//')
            EXPECTED_CLEAN=$(echo "$EXPECTED_VER" | sed 's/-.*//')
            if [ "$INSTALLED_CLEAN" = "$EXPECTED_CLEAN" ]; then
                echo "✓ version matches: $INSTALLED_VER"
            else
                echo "⚠  version mismatch!"
                echo "   Installed binary reports: $INSTALLED_VER"
                echo "   Expected:                $EXPECTED_VER"
                echo "   The old copy may still be in PATH (see conflict check above)."
            fi
        elif [ -n "$INSTALLED_VER" ]; then
            echo "✓ installed: $INSTALLED_VER (source build)"
        fi
    fi
fi

# ── 4. Upgrade in place: a running clari service keeps the OLD binary in
#      memory until restarted. Restart it only if it is already active.
if [ "$DRY" != "1" ] && command -v systemctl >/dev/null 2>&1 \
   && systemctl --user is-active --quiet clari.service 2>/dev/null; then
    if systemctl --user restart clari.service 2>/dev/null; then
        echo "✓ clari.service restarted with the new version"
    else
        echo "⚠  could not restart clari.service — run: systemctl --user restart clari.service"
    fi
fi

echo
echo "Try it:        clari -s"
echo "Background:    clari            (installs + starts the systemd user service)"
echo "Remove it:     clari --uninstall (service) · rm ${INSTALLED_BIN:-~/.local/bin/clari} (binary)"
