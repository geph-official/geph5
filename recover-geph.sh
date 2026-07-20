#!/bin/bash
# Emergency macOS network recovery for geph5 full-tunnel VPN testing.
#
# Kills geph, removes the split-default (/1) routes, and restores DHCP DNS on
# every network service. Idempotent and safe to run at any time. Run as root:
#
#   sudo ./recover-geph.sh
#
# (The macOS analogue of recover-geph.ps1.)
set -u

echo "[recover] killing geph processes..."
# Stop the installed launchd manager first: it has KeepAlive, so plain kills get
# resurrected mid-recovery — and with connected=true persisted it immediately
# re-raises the kill switch this script is trying to clear.
launchctl bootout system /Library/LaunchDaemons/io.geph.manager.plist 2>/dev/null
pkill -9 -f geph5-client 2>/dev/null
pkill -9 -f 'geph5 manager' 2>/dev/null
pkill -9 -f 'geph manager' 2>/dev/null   # installed app's binary is named `geph`
pkill -9 -f 'target/debug/geph5' 2>/dev/null
pkill -9 -f 'target/release/geph5' 2>/dev/null
sleep 1

echo "[recover] deleting split-default routes (if present)..."
# Explicit -netmask/-prefixlen forms: route(8)'s slash-form /1 parsing is not
# trustworthy (its `change` resolves 0.0.0.0/1 to the global default), so stick
# to exact-match syntax for anything touching the top of the routing table.
route -n delete -net 0.0.0.0 -netmask 128.0.0.0    2>/dev/null
route -n delete -net 128.0.0.0 -netmask 128.0.0.0  2>/dev/null
route -n delete -inet6 -net :: -prefixlen 1        2>/dev/null
route -n delete -inet6 -net 8000:: -prefixlen 1    2>/dev/null

echo "[recover] tearing down PF kill switch (if any)..."
pfctl -a geph -F all 2>/dev/null   # flush our anchor's rules
pfctl -f /etc/pf.conf 2>/dev/null  # restore the system's default ruleset
pfctl -d 2>/dev/null               # emergency: force PF off (clears any dangling -E ref)

echo "[recover] restoring DHCP DNS on all services..."
networksetup -listallnetworkservices 2>/dev/null | tail -n +2 | grep -v '^\*' | while IFS= read -r svc; do
  networksetup -setdnsservers "$svc" Empty 2>/dev/null
done
dscacheutil -flushcache 2>/dev/null
killall -HUP mDNSResponder 2>/dev/null

echo "[recover] clearing any geph-planted configd IPv6 state..."
# Remove State:/Network/Service/*/IPv6 dicts carrying geph's tun address
# (2001:db8:6765::). Match on the address, NOT on "InterfaceName : utun":
# other software (iCloud Private Relay, other VPNs) legitimately registers its
# own utuns here, and deleting theirs would break *their* networking. A stale
# geph-planted dict that outlives its utun poisons reachability/DNS system-wide.
echo "list State:/Network/Service/.*/IPv6" | scutil 2>/dev/null \
  | grep -oE 'State:/Network/Service/[^/]+/IPv6' | while IFS= read -r key; do
  if echo "show $key" | scutil 2>/dev/null | grep -q '2001:db8:6765'; then
    echo "[recover] removing $key"
    printf 'remove %s\n' "$key" | scutil 2>/dev/null
  fi
done
# Remove sentinel DNS State overrides (v6 sentinel is geph's marker); configd
# re-derives each service's DNS from DHCP/preferences once the key is gone.
echo "list State:/Network/Service/.*/DNS" | scutil 2>/dev/null \
  | grep -oE 'State:/Network/Service/[^/]+/DNS' | while IFS= read -r key; do
  if echo "show $key" | scutil 2>/dev/null | grep -q '2606:4700:4700::1111'; then
    echo "[recover] removing $key"
    printf 'remove %s\n' "$key" | scutil 2>/dev/null
  fi
done
# Undo any experimental manual-v6 on real services (2001:db8 is geph's marker).
networksetup -listallnetworkservices 2>/dev/null | tail -n +2 | grep -v '^\*' | while IFS= read -r svc; do
  if networksetup -getinfo "$svc" 2>/dev/null | grep -q '2001:db8'; then
    echo "[recover] resetting $svc IPv6 to automatic"
    networksetup -setv6automatic "$svc" 2>/dev/null
  fi
done

echo "[recover] restoring IPv4 default route if missing..."
# While the kill switch was up, configd's router probes were blackholed and macOS
# may have withdrawn the physical default route entirely; without this the machine
# stays offline until a DHCP renewal or reboot. Re-derive the router per active
# interface from DHCP and re-add the default.
if ! netstat -rn | awk '/^Internet:$/{v4=1} /^Internet6:$/{v4=0} v4 && $1=="default" && $4 !~ /^(utun|ipsec)/ {found=1} END{exit !found}'; then
  for ifn in $(ifconfig -l); do
    case "$ifn" in en*) ;; *) continue ;; esac
    router=$(ipconfig getoption "$ifn" router 2>/dev/null)
    if [ -n "${router:-}" ]; then
      echo "[recover] re-adding default via $router ($ifn)"
      route -n add -net default "$router" 2>/dev/null && break
    fi
  done
fi

echo "[recover] done. Checking internet..."
if curl -s --max-time 8 https://ifconfig.me >/dev/null; then
  echo "[recover] internet OK ($(curl -s --max-time 8 https://ifconfig.me))"
else
  echo "[recover] no internet yet — give it a few seconds"
fi
