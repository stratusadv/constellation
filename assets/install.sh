#!/bin/sh
# The one-command Linux install: fetch the latest release, put the binary in
# ~/.local/bin, put that directory on PATH for every future shell, and register
# the MCP server with the agents constellation knows about.
#
#     curl -fsSL https://raw.githubusercontent.com/stratusadv/constellation/main/assets/install.sh | sh
#
# Set CONSTELLATION_INSTALL_DIR to install somewhere other than ~/.local/bin.
#
# POSIX sh, because /bin/sh is dash on Debian and Ubuntu. No bashisms.

set -eu

REPO="stratusadv/constellation"
ASSET="constellation-x86_64-unknown-linux-gnu"
BASE_URL="https://github.com/$REPO/releases/latest/download"
BIN_DIR="${CONSTELLATION_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "constellation: $1" >&2
    exit 1
}

# The published binary is glibc-linked x86_64, so a mismatch here is a wrong
# download rather than a runtime surprise later. Build from source instead.
[ "$(uname -s)" = "Linux" ] || fail "this installer is for Linux; see the README for other platforms"
[ "$(uname -m)" = "x86_64" ] || fail "no prebuilt binary for $(uname -m); build from source with cargo xtask install"

command -v tar >/dev/null 2>&1 || fail "tar is required"

if command -v curl >/dev/null 2>&1; then
    download() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -q -O "$2" "$1"; }
else
    fail "curl or wget is required"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

echo "constellation: downloading $ASSET.tar.gz"

download "$BASE_URL/$ASSET.tar.gz" "$work/$ASSET.tar.gz" \
    || fail "download failed: $BASE_URL/$ASSET.tar.gz"

# The checksum is advisory only in that a release predating it has none. When
# one is published it is enforced, so a corrupted or substituted tarball stops
# here rather than being installed and run.
if download "$BASE_URL/$ASSET.tar.gz.sha256" "$work/$ASSET.tar.gz.sha256" 2>/dev/null \
    && command -v sha256sum >/dev/null 2>&1; then

    (cd "$work" && sha256sum -c "$ASSET.tar.gz.sha256" >/dev/null) \
        || fail "checksum mismatch; refusing to install"

    echo "  checksum verified"
else
    echo "  checksum unavailable, skipped" >&2
fi

tar -xzf "$work/$ASSET.tar.gz" -C "$work" "$ASSET/constellation"

mkdir -p "$BIN_DIR"

# Renamed into place rather than written over: an MCP server started from that
# path may be running, and Linux refuses to open a busy executable for writing
# (ETXTBSY). A rename swaps the directory entry instead, so the update lands
# while the running worker keeps the inode it already opened.
cp "$work/$ASSET/constellation" "$BIN_DIR/constellation.new"
chmod 755 "$BIN_DIR/constellation.new"
mv -f "$BIN_DIR/constellation.new" "$BIN_DIR/constellation"

echo "  binary   $BIN_DIR/constellation"

case "${SHELL:-}" in
    */zsh)  profile="$HOME/.zshrc" ;;
    */bash) profile="$HOME/.bashrc" ;;
    */fish) profile="$HOME/.config/fish/config.fish" ;;
    *)      profile="$HOME/.profile" ;;
esac

case "$profile" in
    */config.fish) path_line="fish_add_path $BIN_DIR" ;;
    *)             path_line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

mkdir -p "$(dirname "$profile")"
touch "$profile"

if grep -qsF "$BIN_DIR" "$profile"; then
    echo "  PATH     already carries $BIN_DIR in $profile"
else
    printf '\n%s\n' "$path_line" >> "$profile"
    echo "  PATH     $BIN_DIR added to $profile"
fi

PATH="$BIN_DIR:$PATH"
export PATH

# Registration reads the running binary's own path, so this must be the copy in
# BIN_DIR and not the one under the temporary directory, which is about to go.
#
# stdin is closed because this script may itself be arriving on stdin from
# `curl | sh`, and a child that read stdin would eat the rest of it.
"$BIN_DIR/constellation" install </dev/null

echo
echo "Open a new shell (or run: $path_line) to pick up the PATH change."
