#!/bin/sh
set -e

# August installer: the core (`august`) and its default extensions (`august-ext-*`).
# Usage: curl -fsSL https://raw.githubusercontent.com/daz9e/august-agent/main/scripts/install.sh | sh
#        curl -fsSL .../install.sh | sh -s -- --ref v0.2.0
#
# Options:
#   --binary       Install the prebuilt release (default)
#   --source       Build with cargo (installs Rust if needed)
#   --ref <ref>    A version of august-agent, e.g. v0.2.0 (source: also a branch or commit);
#                  it brings the core version it was built for
#   -r <ref>       Shorthand for --ref
#   --no-prompt    Ask nothing: update without asking, don't restart (what /update runs)
#
# Releases come from AUGUST_RELEASE_URL (default: this repo's GitHub releases).
# Binaries go to AUGUST_INSTALL_DIR (default ~/.local/bin), all side by side: `august`
# finds its extensions next to itself. Each version of august-agent pins one version of
# the core (Cargo.toml), and a release bundles exactly that core, so the two always match.

REPO="daz9e/august-agent"
CORE_REPO="daz9e/august-core"
INSTALL_DIR="${AUGUST_INSTALL_DIR:-$HOME/.local/bin}"

MODE="binary"
REF=""
NO_PROMPT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --binary) MODE="binary"; shift ;;
        --source) MODE="source"; shift ;;
        --ref|-r)
            [ -n "$2" ] || { echo "Missing value for $1"; exit 1; }
            REF="$2"; shift 2 ;;
        --ref=*) REF="${1#*=}"; shift ;;
        --no-prompt) NO_PROMPT=1; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# Host architecture; on macOS `sysctl` stays right under Rosetta, where `uname -m` lies.
host_arch() {
    if [ "$(uname -s)" = "Darwin" ]; then
        if [ "$(sysctl -in hw.optional.arm64 2>/dev/null)" = "1" ]; then echo aarch64; else echo x86_64; fi
        return
    fi
    case "$(uname -m)" in
        x86_64|amd64) echo x86_64 ;;
        arm64|aarch64) echo aarch64 ;;
        *) uname -m ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

# Moves the binaries in "$1" into INSTALL_DIR, dropping extensions the new release no
# longer has (they would still be started).
put_in_place() {
    mkdir -p "$INSTALL_DIR"
    for old in "$INSTALL_DIR"/august-ext-*; do
        [ -e "$old" ] || continue
        [ -e "$1/$(basename "$old")" ] || rm -f "$old"
    done
    for f in "$1"/august "$1"/august-ext-*; do
        chmod +x "$f"
        mv -f "$f" "$INSTALL_DIR/"
    done
}

install_binary() {
    command -v curl >/dev/null 2>&1 || { echo "curl is required"; exit 1; }
    command -v tar >/dev/null 2>&1 || { echo "tar is required"; exit 1; }
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        echo "sha256sum or shasum is required to verify the download"
        exit 1
    fi
    ARCH="$(host_arch)"
    case "$ARCH" in
        x86_64|aarch64) ;;
        *) echo "Unsupported architecture: $ARCH (try --source)"; exit 1 ;;
    esac
    case "$(uname -s)" in
        Linux) TARGET="${ARCH}-unknown-linux-musl" ;;
        Darwin) TARGET="${ARCH}-apple-darwin" ;;
        *) echo "Unsupported OS: $(uname -s) (try --source)"; exit 1 ;;
    esac

    if [ -n "$AUGUST_RELEASE_URL" ]; then
        BASE="${AUGUST_RELEASE_URL%/}"
    elif [ -n "$REF" ]; then
        BASE="https://github.com/${REPO}/releases/download/${REF}"
    else
        BASE="https://github.com/${REPO}/releases/latest/download"
    fi
    ARCHIVE="august-${TARGET}.tar.gz"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT

    echo "Downloading ${ARCHIVE} (${REF:-latest})..."
    curl -fsSL --connect-timeout 10 --speed-limit 1024 --speed-time 30 "${BASE}/${ARCHIVE}" -o "$TMP/$ARCHIVE" ||
        { echo "No release for ${TARGET} at ${BASE}"; exit 1; }
    curl -fsSL --connect-timeout 10 "${BASE}/${ARCHIVE}.sha256" -o "$TMP/sum" ||
        { echo "No checksum at ${BASE}/${ARCHIVE}.sha256"; exit 1; }
    EXPECTED="$(cut -d ' ' -f 1 < "$TMP/sum")"
    ACTUAL="$(sha256_of "$TMP/$ARCHIVE")"
    if [ "$EXPECTED" != "$ACTUAL" ]; then
        echo "Checksum mismatch for ${ARCHIVE}: expected ${EXPECTED}, got ${ACTUAL}"
        exit 1
    fi

    mkdir "$TMP/out"
    tar -xzf "$TMP/$ARCHIVE" -C "$TMP/out" --strip-components 1
    [ -f "$TMP/out/august" ] || { echo "The archive has no august binary"; exit 1; }
    # Never report success for a binary that can't start.
    if ! OUT="$("$TMP/out/august" help 2>&1)"; then
        echo "✗ The downloaded august cannot start:"
        echo "$OUT" | sed 's/^/    /'
        exit 1
    fi
    VERSIONS="$(cat "$TMP/out/VERSIONS" 2>/dev/null || true)"
    rm -f "$TMP/out/VERSIONS"
    put_in_place "$TMP/out"
}

install_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Installing Rust..."
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
        . "$HOME/.cargo/env"
    fi
    command -v git >/dev/null 2>&1 || { echo "git is required for --source"; exit 1; }
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT

    echo "Fetching august-agent ${REF:-main}..."
    git clone --quiet "https://github.com/${REPO}.git" "$TMP/agent"
    [ -z "$REF" ] || git -C "$TMP/agent" checkout --quiet "$REF"
    # The core at the commit the lockfile pins the SDK to.
    CORE_REV="$(sed -n "s|^source = \"git+https://github.com/${CORE_REPO}[^#]*#\([0-9a-f]*\)\"|\1|p" "$TMP/agent/Cargo.lock" | head -n 1)"
    [ -n "$CORE_REV" ] || { echo "Could not find the core's commit in Cargo.lock"; exit 1; }

    echo "Building the extensions..."
    cargo install --locked --path "$TMP/agent" --root "$TMP/root"
    echo "Building the core (${CORE_REV})..."
    cargo install --locked --git "https://github.com/${CORE_REPO}" --rev "$CORE_REV" august --root "$TMP/root"
    put_in_place "$TMP/root/bin"
    VERSIONS="august-agent $(sed -n 's/^version = "\(.*\)"/\1/p' "$TMP/agent/Cargo.toml" | head -n 1)
august-core ${CORE_REV}"
}

# Questions go to the terminal: stdin is this script, piped from curl. Without a terminal
# (or with --no-prompt) nothing is asked.
can_ask() { [ -z "$NO_PROMPT" ] && ( true < /dev/tty ) 2>/dev/null; }
ask() {
    printf '%s [Y/n] ' "$1"
    read -r answer < /dev/tty || answer=""
    case "$answer" in [nN]*) return 1 ;; esac
}

if [ -x "$INSTALL_DIR/august" ] && can_ask && ! ask "August is already installed in ${INSTALL_DIR}. Update it to ${REF:-the latest version}?"; then
    echo "Nothing changed."
    exit 0
fi

case "$MODE" in
    binary) install_binary ;;
    source) install_source ;;
esac

echo ""
echo "✓ Installed august and its extensions to ${INSTALL_DIR}"
[ -z "$VERSIONS" ] || echo "$VERSIONS" | sed 's/^/  /'
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "  Add ${INSTALL_DIR} to your PATH first." ;;
esac
cat <<EOF

Next:
  august serve            start it in the background
  august                  open the chat in this terminal, then:
    /login                pick a model provider and sign in
    /login telegram       connect a Telegram bot (token from @BotFather)
    /login discord        connect a Discord bot (token from the Developer Portal)

EOF
if [ -S "${AUGUST_HOME:-$HOME/.august}/control.sock" ]; then
    if can_ask && ask "August is running. Restart it now to run the new version?"; then
        "$INSTALL_DIR/august" restart
    else
        echo "Already running? Pick up the new version with: august restart"
    fi
fi
