#!/usr/bin/env sh
# Hedge @VERSION@ — self-extracting installer.
#
# This file is a shell script with a .tar.gz stapled to the end of it: the
# header below unpacks the payload to a temp dir and runs the binary's own
# --install. Pass any hedge flag through, e.g. `--uninstall` or `--status`.
set -eu

MARKER_LINE=$(awk '/^__HEDGE_PAYLOAD__$/ { print NR + 1; exit }' "$0")
TMP=$(mktemp -d "${TMPDIR:-/tmp}/hedge-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

tail -n "+$MARKER_LINE" "$0" | tar xz -C "$TMP"
BIN=$(find "$TMP" -type f -name hedge -perm -u+x | head -1)
[ -n "$BIN" ] || { echo "hedge: corrupt installer payload" >&2; exit 1; }

# not exec: the EXIT trap has to survive to clear the temp dir
if [ "$#" -eq 0 ]; then
    set -- --install
fi
"$BIN" "$@"
status=$?
# Stop before the payload — the archive below is data, not script. The EXIT
# trap still runs on an explicit exit, so the temp dir is cleaned up.
exit "$status"

__HEDGE_PAYLOAD__
