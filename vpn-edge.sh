#!/bin/bash
# Edge-case checks for macOS VPN: sentinel DNS applied, reconnect keeps the tunnel
# up (fd re-dup into the new engine child), disconnect leaves a clean state, and a
# second connect cycle still works. Self-contained; backstop watchdog at 150s.
#   sudo ./vpn-edge.sh
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
# The daemon auto-stages the engine binary, so run straight from target/debug.
GEPH="$DIR/target/debug/geph5"
LOG=/tmp/geph-daemon.log; R=/tmp/geph-edge.txt; : > "$R"
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }

nohup bash -c "sleep 150; '$DIR/recover-geph.sh'" >/tmp/geph-watchdog.log 2>&1 &
echo "[edge] backstop watchdog PID $! (150s)"
pkill -9 -f 'target/debug/geph5' 2>/dev/null; sleep 1
: > "$LOG"
RUST_LOG=geph=info nohup "$GEPH" daemon >>"$LOG" 2>&1 &
echo "[edge] daemon PID $!"; sleep 3
ip(){ curl -s --max-time 10 https://api.ipify.org; }
utun(){ ifconfig 2>/dev/null | awk '/^utun/{i=$1} /100\.64\.0\.1/{print i}'; }

"$GEPH" vpn on >/dev/null 2>&1; "$GEPH" connect >/dev/null 2>&1
echo "[edge] session 1 bootstrapping..."; sleep 15
{
  echo "== session 1 =="
  "$GEPH" status 2>&1 | grep -E 'State|Exit'
  echo "sentinel DNS: $(scutil --dns 2>/dev/null | awk '/nameserver\[0\]/{print $3; exit}')"
  echo "utun: $(utun)   exit IP: $(ip)"

  echo "== reconnect (engine restart, tunnel should persist) =="
  "$GEPH" reconnect >/dev/null 2>&1; sleep 12
  echo "$(\"$GEPH\" status 2>&1 | grep State)   utun: $(utun)   exit IP: $(ip)"

  echo "== disconnect (expect clean) =="
  "$GEPH" disconnect >/dev/null 2>&1; sleep 2
  echo "/1 routes left: $(netstat -rn -f inet 2>/dev/null | grep -cE '^(0/1|128)')   utun left: '$(utun)'"
  echo "ifscope default left: $(netstat -rn -f inet 2>/dev/null | grep -i default | grep -c 'I.*en1')"
  echo "real IP: $(ip)"

  echo "== reconnect cycle 2 (fresh utun) =="
  "$GEPH" connect >/dev/null 2>&1; sleep 14
  echo "utun: $(utun)   exit IP: $(ip)"
} >>"$R" 2>&1

"$GEPH" disconnect >/dev/null 2>&1; "$DIR/recover-geph.sh" >/dev/null 2>&1
pkill -f "sleep 150" 2>/dev/null
echo "================= EDGE RESULTS ================="; cat "$R"
echo "real IP after full revert: $(ip)"
echo "==============================================="
