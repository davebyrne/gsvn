#!/bin/bash
# Run a command (usually the gsvn binary) inside a private headless mutter
# with its own D-Bus session, so it neither touches the desktop nor gets
# forwarded to an already-running gsvn instance (GApplication single-instance).
#
# Usage: scripts/headless.sh <command...>
#   e.g. GSVN_DEBUG_DIFF="http://localhost:8080/svn/small 23" GSVN_DIFF_SHOT=/tmp/d.png \
#        scripts/headless.sh target/debug/gsvn
#
# Settings go to a throwaway GSVN_CONFIG_DIR unless one is set.
set -eu
export GTK_A11Y=none
export GSVN_CONFIG_DIR=${GSVN_CONFIG_DIR:-$(mktemp -d)}
exec dbus-run-session -- bash -c '
display=gsvn-headless-$$
log=$(mktemp)
mutter --headless --wayland --no-x11 --wayland-display=$display --virtual-monitor 1400x900 >"$log" 2>&1 &
mp=$!
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$display" ] && break; sleep 0.1; done
WAYLAND_DISPLAY=$display GDK_BACKEND=wayland "$@"
rc=$?
kill $mp
exit $rc
' bash "$@"
