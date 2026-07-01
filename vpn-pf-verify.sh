#!/bin/bash
# Verify the PF kill switch: engine runs as _geph and is permitted (tunnel works),
# the anchor is loaded, and a non-engine direct-egress attempt on the physical NIC
# is BLOCKED (leak test). Then fully revert. Backstop watchdog at 120s.
#   sudo ./vpn-pf-verify.sh
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
# The manager auto-stages the engine binary to a world-traversable, root-owned dir,
# so we can run straight from target/debug (under the 0750 home) with no install.
GEPH="$DIR/target/debug/geph5"
LOG=/tmp/geph-manager.log; R=/tmp/geph-pf.txt; : > "$R"
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }

nohup bash -c "sleep 120; '$DIR/recover-geph.sh'" >/tmp/geph-watchdog.log 2>&1 &
echo "[pf] backstop watchdog PID $! (120s)"
pkill -9 -f 'target/debug/geph5' 2>/dev/null; sleep 1
: > "$LOG"
RUST_LOG=geph=info nohup "$GEPH" manager >>"$LOG" 2>&1 &
echo "[pf] manager PID $!"; sleep 3

"$GEPH" vpn on >/dev/null 2>&1; "$GEPH" connect >/dev/null 2>&1
echo "[pf] bootstrapping (network blips)..."; sleep 16

leak_test(){ python3 - <<'PY'
import socket
try:
    s=socket.socket(); s.setsockopt(socket.IPPROTO_IP,25,socket.if_nametoindex("en1"))
    s.settimeout(5); s.connect(("1.1.1.1",443)); print("REACHED -- LEAK!"); s.close()
except Exception as e:
    print("blocked (good):", e)
PY
}

{
  echo "== status =="; "$GEPH" status 2>&1 | grep -E 'State|Exit'
  echo "== engine uid (want _geph) =="; ps -axo user,comm | grep geph5-client | grep -v grep | sort -u
  echo "== pf status =="; pfctl -s info 2>/dev/null | grep -i status
  echo "== geph anchor rules =="; pfctl -a geph -sr 2>/dev/null
  echo -n "== exit IP (engine permitted + tunnel) == "; curl -s --max-time 12 https://api.ipify.org; echo
  echo -n "== LEAK TEST root direct-egress en1 == "; leak_test
} >>"$R" 2>&1

"$GEPH" disconnect >/dev/null 2>&1; sleep 1
{
  echo "== after disconnect =="
  echo "pf status: $(pfctl -s info 2>/dev/null | grep -i -o 'Status: [A-Za-z]*')"
  echo "geph anchor rules: $(pfctl -a geph -sr 2>/dev/null | wc -l | tr -d ' ') lines"
} >>"$R" 2>&1
"$DIR/recover-geph.sh" >/dev/null 2>&1
pkill -f "sleep 120" 2>/dev/null

echo "================= PF VERIFY ================="; cat "$R"
echo -n "real IP after revert: "; curl -s --max-time 8 https://api.ipify.org; echo
echo "============================================"
