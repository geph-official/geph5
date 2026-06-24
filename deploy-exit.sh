#!/bin/bash
set -e

########################################################################
# 0. Handle auth-token input
########################################################################
if [ -z "$AUTH_TOKEN" ]; then
  read -p "Enter the auth_token: " auth_token
else
  auth_token="$AUTH_TOKEN"
fi

########################################################################
# 1. sysctl tuning
########################################################################
echo -e "\033[1mSetting up sysctl\033[0m"
tee /etc/sysctl.conf << 'EOF'
net.ipv4.tcp_congestion_control = bbr
net.core.default_qdisc          = fq
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_syncookies         = 1
net.ipv4.ip_local_port_range    = 1024 65535
net.ipv4.tcp_mtu_probing = 1
EOF
sysctl -p

########################################################################
# 2. Packages & binaries
########################################################################
echo -e "\033[1mUpdating package lists and installing dependencies\033[0m"
apt-get update || true
apt-get install -y curl jq iputils-ping

echo -e "\033[1mDownloading geph5-exit\033[0m"
curl -s https://artifacts.geph.io/musl-latest/geph5-exit -o /usr/local/bin/geph5-exit
chmod +x /usr/local/bin/geph5-exit

########################################################################
# 2.5 MTU discovery (binary search, ICMP-blackhole-resistant)
########################################################################
# Path MTU is discovered by sending DF-set ICMP echoes of increasing size to
# 1.1.1.1 and binary-searching for the largest packet that gets through. We do
# NOT rely on receiving "fragmentation needed" replies (those are often dropped
# by ICMP blackholes): a probe "succeeds" only if we get an echo *reply* back,
# and "fails" (too big) on any other outcome. The result is then pinned onto the
# primary interface persistently via a systemd unit.
echo -e "\033[1mDiscovering path MTU via binary search\033[0m"

# IP + ICMP headers consume 28 bytes; ping -s sets the payload size, so the
# on-wire packet size is payload + 28. We search in packet-size (MTU) space.
ICMP_OVERHEAD=28
PING_TARGET=1.1.1.1

# Returns 0 if a DF-set packet of the given total MTU size reaches the target.
mtu_probe() {
  local mtu="$1"
  local payload=$((mtu - ICMP_OVERHEAD))
  # -M do: set DF, never fragment. -c 1: one packet. -W 2: 2s reply timeout.
  ping -M do -s "$payload" -c 1 -W 2 "$PING_TARGET" >/dev/null 2>&1
}

discovered_mtu=""
# Only bother if the target is reachable at all at a safe, universally-routable
# size (1280 = the IPv6 minimum, which virtually every path supports).
if mtu_probe 1280; then
  lo=1280   # known-good
  hi=1500   # standard Ethernet ceiling; never probe above this
  # If even 1500 works, we're done — that's the max we'd ever set.
  if mtu_probe "$hi"; then
    discovered_mtu=$hi
  else
    # Binary-search the largest size in (lo, hi) that still gets through.
    while [ $((hi - lo)) -gt 1 ]; do
      mid=$(((lo + hi) / 2))
      if mtu_probe "$mid"; then
        lo=$mid
      else
        hi=$mid
      fi
    done
    discovered_mtu=$lo
  fi
fi

if [ -n "$discovered_mtu" ]; then
  primary_iface=$(ip -o route get "$PING_TARGET" 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n1)
fi

if [ -n "$discovered_mtu" ] && [ -n "$primary_iface" ]; then
  echo -e "\033[1mDiscovered MTU $discovered_mtu on $primary_iface; applying and persisting\033[0m"
  ip link set dev "$primary_iface" mtu "$discovered_mtu" || true

  # Persist across reboots with a oneshot unit, independent of the network
  # manager in use. Re-applies on every boot after the interface is up.
  cat > /etc/systemd/system/geph5-mtu.service <<EOL
[Unit]
Description=Pin path MTU for geph5 exit
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=/sbin/ip link set dev $primary_iface mtu $discovered_mtu
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOL
  systemctl daemon-reload
  systemctl enable --now geph5-mtu.service || true
else
  echo -e "\033[1mCould not determine MTU (target unreachable?); leaving MTU unchanged\033[0m"
fi

########################################################################
# 3. Prep directories & random ports
########################################################################
echo -e "\033[1mCreating configuration directory\033[0m"
mkdir -p /etc/geph5-exit

echo -e "\033[1mGenerating random port numbers\033[0m"
c2e_port=$((10000 + RANDOM % 55535))
b2e_port=$((10000 + RANDOM % 55535))

########################################################################
# 4. Geo-location lookup
########################################################################
echo -e "\033[1mGetting country and city using ip-api.com\033[0m"
location_data=$(curl -s 'http://ip-api.com/json/')
country=$(echo "$location_data" | jq -r '.countryCode')
city=$(echo "$location_data"    | jq -r '.city')

########################################################################
# 5. Exit-metadata logic (NEW)
########################################################################
# Default values
allowed_levels='[Free, Plus]'
category='core'

# Override for PLUS_ONLY and STREAMING env flags
if [ -n "$PLUS_ONLY" ]; then
  allowed_levels='[Plus]'
fi

if [ -n "$STREAMING" ]; then
  category='streaming'
fi

########################################################################
# 6. Signing secret
########################################################################
echo -e "\033[1mGenerating signing secret\033[0m"
dd if=/dev/random of=/etc/geph5-exit/signing.secret bs=1 count=32 2>/dev/null
chmod 600 /etc/geph5-exit/signing.secret

########################################################################
# 7. Config YAML
########################################################################
if [ ! -f /etc/geph5-exit/config.yaml ]; then
  echo -e "\033[1mCreating configuration file\033[0m"
  cat > /etc/geph5-exit/config.yaml <<EOL
broker:
  url: https://broker.geph.io/
  auth_token: $auth_token

c2e_listen: 0.0.0.0:$c2e_port
b2e_listen: 0.0.0.0:$b2e_port
country: $country
city: $city

metadata:
  allowed_levels: $allowed_levels
  category: $category

signing_secret: /etc/geph5-exit/signing.secret
EOL
else
  echo -e "\033[1mConfiguration file already exists. Skipping creation.\033[0m"
fi

########################################################################
# 8. systemd service & timer
########################################################################
echo -e "\033[1mCreating systemd service file\033[0m"
cat > /etc/systemd/system/geph5-exit.service <<'EOL'
[Unit]
Description=Geph5 Exit
After=network.target
StartLimitIntervalSec=0
StartLimitBurst=0

[Service]
ExecStart=/usr/local/bin/geph5-exit --config /etc/geph5-exit/config.yaml
Restart=always
RestartSec=5
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOL

echo -e "\033[1mCreating systemd timer file for upgrades\033[0m"
cat > /etc/systemd/system/geph5-exit-upgrade.timer <<'EOL'
[Unit]
Description=Geph5 Exit Upgrade Timer

[Timer]
OnCalendar=hourly
Persistent=true

[Install]
WantedBy=timers.target
EOL

echo -e "\033[1mCreating systemd service file for upgrades\033[0m"
cat > /etc/systemd/system/geph5-exit-upgrade.service <<'EOL'
[Unit]
Description=Geph5 Exit Upgrade Service
After=network-online.target

[Service]
Type=oneshot
ExecStart=/bin/bash -c "curl -f -s https://pub-a02e86bb722a4b079127aeee6d399e5d.r2.dev/musl-latest/geph5-exit -o /tmp/geph5-exit && \
if ! cmp -s /tmp/geph5-exit /usr/local/bin/geph5-exit; then \
    mv /tmp/geph5-exit /usr/local/bin/geph5-exit && \
    chmod +x /usr/local/bin/geph5-exit && \
    systemctl restart geph5-exit; \
  else \
    rm /tmp/geph5-exit; \
  fi"

[Install]
WantedBy=multi-user.target
EOL

########################################################################
# 9. Activate everything
########################################################################
echo -e "\033[1mReloading systemd and starting services\033[0m"
systemctl daemon-reload
systemctl enable --now geph5-exit
systemctl enable --now geph5-exit-upgrade.timer
