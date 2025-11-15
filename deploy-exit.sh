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
EOF
sysctl -p

########################################################################
# 2. Packages & binaries
########################################################################
echo -e "\033[1mUpdating package lists and installing dependencies\033[0m"
apt-get update || true
apt-get install -y curl jq

echo -e "\033[1mDownloading geph5-exit\033[0m"
curl -s https://artifacts.geph.io/musl-latest/geph5-exit -o /usr/local/bin/geph5-exit
chmod +x /usr/local/bin/geph5-exit

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
ExecStart=/bin/bash -c "curl -s https://artifacts.geph.io/musl-latest/geph5-exit -o /tmp/geph5-exit && \
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
