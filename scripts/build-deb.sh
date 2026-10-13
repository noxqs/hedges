#!/usr/bin/env bash
# Build the Debian package.
#
# Produces, in dist/:
#   hedge_<version>-<stamp>_<arch>.deb
#
# The .deb is the system-wide flavour of what `hedge --install` does per-user:
# the binary in /usr/bin, autostart for every account via /etc/xdg/autostart,
# the app-menu entry, the icon, and the Nemo right-click actions. The binary
# notices it is running from /usr (install::packaged) and never offers to
# self-install on top.
#
# The build stamp is pinned via SOURCE_DATE_EPOCH so the time baked into the
# binary (what the upgrade check compares) is exactly the time in the file name.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
: "${SOURCE_DATE_EPOCH:=$(date -u +%s)}"
export SOURCE_DATE_EPOCH
STAMP="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y%m%d%H%M)"
ARCH="$(dpkg --print-architecture)"
PKGVER="${VERSION}-${STAMP}"
NAME="hedge_${PKGVER}_${ARCH}"

echo "==> Hedge $VERSION  (deb $PKGVER, $ARCH)"

# --- checks -----------------------------------------------------------------
command -v cargo >/dev/null || { echo "Missing the Rust toolchain (rustup)." >&2; exit 1; }
command -v dpkg-deb >/dev/null || { echo "Missing dpkg-deb." >&2; exit 1; }
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
PKG="$DIST/$NAME"
rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" \
         "$PKG/usr/bin" \
         "$PKG/usr/share/applications" \
         "$PKG/usr/share/icons/hicolor/scalable/apps" \
         "$PKG/usr/share/nemo/actions" \
         "$PKG/usr/share/doc/hedge" \
         "$PKG/etc/xdg/autostart"

install -m755 "$BIN" "$PKG/usr/bin/hedge"
install -m644 "$ROOT/assets/icons/hedge.svg" \
    "$PKG/usr/share/icons/hicolor/scalable/apps/hedge.svg"

# The same four integration files install.rs writes per-user, at their
# system-wide homes and pointing at /usr/bin. Keep them in step with install.rs.
cat > "$PKG/etc/xdg/autostart/hedge.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Hedge
Comment=Desktop app launcher
Exec=/usr/bin/hedge
Icon=hedge
X-GNOME-Autostart-enabled=true
NoDisplay=true
EOF

cat > "$PKG/usr/share/applications/hedge-settings.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Hedge Settings
Comment=Configure your Hedges
Exec=/usr/bin/hedge --settings
Icon=hedge
Terminal=false
Categories=Settings;Utility;
EOF

cat > "$PKG/usr/share/nemo/actions/hedge.nemo_action" <<'EOF'
[Nemo Action]
Name=Add a Hedge
Comment=Add a new Hedge to the desktop
Exec=/usr/bin/hedge --add
Icon-Name=list-add
Selection=None
Extensions=any;
EOF

cat > "$PKG/usr/share/nemo/actions/hedge-toggle-desktop.nemo_action" <<'EOF'
[Nemo Action]
Name=Toggle desktop icons
Comment=Hide or show the desktop icons
Exec=/usr/bin/hedge --toggle-desktop
Icon-Name=view-hidden
Selection=None
Extensions=any;
EOF

cat > "$PKG/usr/share/doc/hedge/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: hedge

Files: *
Copyright: $(date -u -d "@$SOURCE_DATE_EPOCH" +%Y) Morgan
License: MIT
EOF

# --- control ----------------------------------------------------------------
SIZE_KB="$(du -sk "$PKG" --exclude=DEBIAN | cut -f1)"

# GTK3 pulls glib/cairo/pango/gdk-pixbuf in as its own dependencies, so one
# runtime Depends covers the lot. wmctrl is only click-to-focus on the Claude
# rows; traceroute only the ping desklet's path view — degrade, don't require.
cat > "$PKG/DEBIAN/control" <<EOF
Package: hedge
Version: $PKGVER
Section: utils
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE_KB
Depends: libgtk-3-0 (>= 3.22)
Recommends: wmctrl
Suggests: nemo, traceroute
Maintainer: Morgan <targhan@gmail.com>
Description: Zen, drag-friendly app launcher and desklets for the Linux desktop
 Hedge draws movable launcher hedges and desklets (clock, system monitor,
 ping, Claude Code sessions) directly on the desktop. This package installs
 Hedge system-wide: it starts at login for every account, and adds the
 "Hedge Settings" menu entry and the Nemo desktop right-click actions.
EOF

# the autostart entry lives in /etc so a local edit must survive upgrades
echo "/etc/xdg/autostart/hedge.desktop" > "$PKG/DEBIAN/conffiles"

cat > "$PKG/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qf /usr/share/icons/hicolor || true
EOF

cat > "$PKG/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qf /usr/share/icons/hicolor || true
EOF

chmod 755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm"

# the staging tree inherits the build user's umask; a package must not ship
# group-writable paths
find "$PKG" -type d -exec chmod 755 {} +
find "$PKG" -type f ! -perm -111 -exec chmod 644 {} +
chmod 755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm" "$PKG/usr/bin/hedge"

# --- package ----------------------------------------------------------------
mkdir -p "$DIST"
# only ever one .deb in dist, like build-release.sh keeps one tarball
rm -f "$DIST"/hedge_*.deb
DEB="$DIST/$NAME.deb"
dpkg-deb --build --root-owner-group "$PKG" "$DEB" >/dev/null
rm -rf "$PKG"
echo "==> $DEB"

# append to the SHA256SUMS build-release.sh starts (or start one), replacing
# any line a previous run of this script left for an older .deb
( cd "$DIST" \
  && { grep -v '\.deb$' SHA256SUMS 2>/dev/null || true; sha256sum "$NAME.deb"; } > SHA256SUMS.tmp \
  && mv SHA256SUMS.tmp SHA256SUMS )

command -v lintian >/dev/null && lintian --no-tag-display-limit "$DEB" || true

cat <<EOF

Done. To install:
  sudo apt install $DEB

Hedge then starts at the next login (or run 'hedge' now). Remove with
'sudo apt remove hedge'. A per-user install made earlier by 'hedge
--install' is separate — remove that one from Hedge Settings, or with
'hedge --uninstall' before the package's copy takes over at login.
EOF
