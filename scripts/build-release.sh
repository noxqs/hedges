#!/usr/bin/env bash
# Build the distributable Hedge installer.
#
# Produces, in dist/:
#   hedge-<version>-<stamp>-<arch>-linux.tar.gz   the binary (it self-installs)
#   hedge-<version>-<stamp>-<arch>-linux.run      one-file self-extracting installer
#   SHA256SUMS                                    checksums for both
#
# The build stamp is pinned via SOURCE_DATE_EPOCH so the time baked into the
# binary (what the upgrade check compares) is exactly the time in the file name.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
: "${SOURCE_DATE_EPOCH:=$(date -u +%s)}"
export SOURCE_DATE_EPOCH
STAMP="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y%m%d-%H%M)"
ARCH="$(uname -m)"
NAME="hedge-${VERSION}-${STAMP}-${ARCH}-linux"

echo "==> Hedge $VERSION  (build stamp $STAMP UTC, $ARCH)"

# --- checks -----------------------------------------------------------------
command -v cargo >/dev/null || { echo "Missing the Rust toolchain (rustup)." >&2; exit 1; }
pkg-config --exists gtk+-3.0 2>/dev/null || {
    echo "Missing GTK3 dev libraries — see the README for the apt line." >&2; exit 1; }

# --- build ------------------------------------------------------------------
echo "==> cargo test"
( cd "$ROOT" && cargo test --release --quiet )
echo "==> cargo build --release"
( cd "$ROOT" && cargo build --release )

BIN="$ROOT/target/release/hedge"
command -v strip >/dev/null && strip "$BIN" || true
echo "==> binary: $("$BIN" --version)"

# --- stage ------------------------------------------------------------------
# clear the previous run: SHA256SUMS only ever covers what's in here now
rm -rf "$DIST"
mkdir -p "$DIST/$NAME"
install -m755 "$BIN" "$DIST/$NAME/hedge"
[ -f "$ROOT/README.md" ] && cp "$ROOT/README.md" "$DIST/$NAME/"

# No install script in the payload on purpose: the binary *is* the installer.
# `./hedge --install` is the whole story, and a wrapper around it would be a
# second place for the file list to drift out of.
cat > "$DIST/$NAME/README-INSTALL.txt" <<EOF
Hedge $VERSION (built $STAMP UTC)

Install into your home directory — no root, no package manager:

    ./hedge --install

Other things this same binary does:

    ./hedge --status         what's installed vs. this build
    ./hedge --uninstall      remove it (add --purge to drop the config too)
    ./hedge                  just run it, without installing anything

Installing registers autostart, the "Hedge Settings" menu entry and the
desktop right-click actions; uninstalling removes exactly those again.
EOF

# --- package ----------------------------------------------------------------
TAR="$DIST/$NAME.tar.gz"
tar czf "$TAR" -C "$DIST" "$NAME"
echo "==> $TAR"

RUN="$DIST/$NAME.run"
{
    sed "s/@VERSION@/$VERSION $STAMP UTC/" "$ROOT/scripts/self-extract-header.sh"
    cat "$TAR"
} > "$RUN"
chmod +x "$RUN"
echo "==> $RUN"

( cd "$DIST" && sha256sum "$NAME.tar.gz" "$NAME.run" > SHA256SUMS )
rm -rf "$DIST/$NAME"

cat <<EOF

Done. To install from here:
  $RUN

To install elsewhere, copy that one file over and run it — or unpack the
tarball and run './hedge --install'. Either way the binary installs itself:
everything under \$HOME, including the desktop right-click actions.
EOF
