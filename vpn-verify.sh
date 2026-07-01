#!/bin/bash
# End-to-end macOS VPN verification: bring the tunnel up, check the utun/routes,
# confirm traffic egresses the exit (public IP changes) and DNS resolves through
# the tunnel, then FULLY revert. Self-contained; backstop watchdog at 120s.
#   sudo ./vpn-verify.sh [bootstrap_wait]   # default 15
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
GEPH="$DIR/target/debug/geph5"
LOG=/tmp/geph-manager.log
R=/tmp/geph-verify.txt; : > "$R"
WAIT="${1:-15}"

[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }

nohup bash -c "sleep 120; '$DIR/recover-geph.sh'" >/tmp/geph-watchdog.log 2>&1 &
echo "[verify] backstop watchdog PID $! (120s)"

pkill -9 -f 'target/debug/geph5' 2>/dev/null; sleep 1
: > "$LOG"
RUST_LOG=geph=info nohup "$GEPH" manager >>"$LOG" 2>&1 &
echo "[verify] manager PID $!"; sleep 3

"$GEPH" vpn on >/dev/null 2>&1
"$GEPH" connect >/dev/null 2>&1
echo "[verify] connecting; waiting ${WAIT}s for bootstrap (network blips)..."
sleep "$WAIT"

{
  echo "=== status ==="; "$GEPH" status 2>&1
  echo "=== geph utun ==="; ifconfig 2>/dev/null | awk '/^utun/{i=$1} /100\.64\.0\.1/{print i": "$0}'
  echo "=== /1 routes ==="; netstat -rn -f inet 2>/dev/null | grep -E '^(0/1|128)'
  echo "=== scoped default (en1) ==="; netstat -rn -f inet 2>/dev/null | grep -i 'default' | grep -i en1
  echo -n "=== public IP through tunnel === "; curl -s --max-time 12 https://api.ipify.org 2>&1; echo
  echo -n "=== DNS+HTTPS through tunnel (want 200) === "; curl -s --max-time 12 -o /dev/null -w '%{http_code}' https://example.com 2>&1; echo
} >>"$R" 2>&1

echo "[verify] reverting..."
"$GEPH" disconnect >/dev/null 2>&1
"$DIR/recover-geph.sh" >/dev/null 2>&1

echo "================= VERIFY RESULTS ================="
cat "$R"
echo -n "=== real IP after revert === "; curl -s --max-time 8 https://api.ipify.org 2>&1; echo
echo "================================================="
