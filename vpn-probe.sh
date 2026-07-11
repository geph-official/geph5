#!/bin/bash
# One-shot macOS VPN diagnostic: start the manager, bring the tunnel up, capture a
# few seconds of logs, then FULLY revert — all in a single root invocation. The
# network is only captured briefly and self-restores at the end, so the captured
# log returns in this command's output. An independent backstop watchdog is also
# armed in case the script is interrupted.
#   sudo ./vpn-probe.sh [capture_seconds]      # default 18
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
GEPH="$DIR/target/debug/geph5"
ENGINE="$DIR/target/debug/geph5-client"
LOG=/tmp/geph-manager.log
WAIT="${1:-18}"

[ "$(id -u)" -eq 0 ] || { echo "must run as root (sudo)"; exit 1; }

# Backstop: revert no matter what after 120s, even if this script is killed.
nohup bash -c "sleep 120; '$DIR/recover-geph.sh'" >/tmp/geph-watchdog.log 2>&1 &
echo "[probe] backstop watchdog PID $! (120s)"

# Fresh manager.
pkill -9 -f 'target/debug/geph5' 2>/dev/null; sleep 1
: > "$LOG"
GEPH_CLIENT_BIN="$ENGINE" RUST_LOG=geph=debug nohup "$GEPH" manager >>"$LOG" 2>&1 &
echo "[probe] manager PID $!"
sleep 3

echo "[probe] enabling VPN + connecting..."
"$GEPH" vpn on || true
"$GEPH" connect || true
echo "[probe] capturing ${WAIT}s of logs (network may blip here)..."
sleep "$WAIT"

echo "[probe] reverting..."
"$GEPH" disconnect 2>/dev/null || true
"$DIR/recover-geph.sh" >/dev/null 2>&1

echo "============== RELEVANT LOG LINES =============="
grep -nE "IP_BOUND_IF|forwarder|auth loop|broker source|vpn setup|physical|utun|bind_if|EBADF|os error" "$LOG" | tail -n 70
echo "============== (full log: $LOG) ================"
echo "[probe] done; network reverted."
