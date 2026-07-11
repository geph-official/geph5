#!/bin/bash
# Launch the geph5 manager for macOS VPN testing behind an INDEPENDENT self-destruct
# timer that reverts ALL network state after TIMEOUT seconds, no matter what the
# manager does (crash, hang, blackhole). This is the safety net: even if the tunnel
# wedges connectivity, the watchdog restores it on its own.
#
#   sudo ./vpn-test.sh [timeout_seconds]     # default 180
#
# After it returns, drive the manager WITHOUT sudo:
#   ./target/debug/geph5 status | vpn on | connect | disconnect
# Revert immediately at any time:
#   sudo ./recover-geph.sh
set -u

DIR="$(cd "$(dirname "$0")" && pwd)"
TIMEOUT="${1:-180}"
GEPH="$DIR/target/debug/geph5"
ENGINE="$DIR/target/debug/geph5-client"
LOG=/tmp/geph-manager.log

[ "$(id -u)" -eq 0 ] || { echo "must run as root (sudo)"; exit 1; }
[ -x "$GEPH" ]   || { echo "missing $GEPH (cargo build first)"; exit 1; }
[ -x "$ENGINE" ] || { echo "missing $ENGINE (cargo build first)"; exit 1; }

# Arm the watchdog FIRST, fully detached, so it fires regardless of this script or
# the manager. It just runs recover-geph.sh after the timeout.
echo "[arm] watchdog will revert network in ${TIMEOUT}s no matter what"
nohup bash -c "sleep $TIMEOUT; echo '[watchdog] firing self-destruct'; '$DIR/recover-geph.sh'" \
  >/tmp/geph-watchdog.log 2>&1 &
echo $! >/tmp/geph-watchdog.pid
echo "[arm] watchdog PID $(cat /tmp/geph-watchdog.pid)  (log: /tmp/geph-watchdog.log)"

# Start the manager in the background; logs to $LOG.
echo "[manager] starting (log: $LOG)"
: > "$LOG"
GEPH_CLIENT_BIN="$ENGINE" RUST_LOG=geph=debug nohup "$GEPH" manager >>"$LOG" 2>&1 &
echo $! >/tmp/geph-manager.pid
echo "[manager] PID $(cat /tmp/geph-manager.pid)"
sleep 2
echo "[manager] recent log:"; tail -n 10 "$LOG" | sed 's/^/    /'
echo
echo "Daemon up. Drive it (no sudo):  $GEPH status / vpn on / connect / disconnect"
echo "Revert now:                      sudo $DIR/recover-geph.sh"
echo "Cancel the watchdog early:       sudo kill \$(cat /tmp/geph-watchdog.pid)"
