#!/bin/bash

if [ -z "$AUTH_TOKEN" ]; then
  read -p "Enter the auth_token: " auth_token
else
  auth_token="$AUTH_TOKEN"
fi

echo -e "\033[1mSetting up sysctl\033[0m"
tee /etc/sysctl.conf << EOF
net.core.rmem_default=262144
net.core.wmem_default=262144
net.core.rmem_max=262144000
net.core.wmem_max=262144000
net.ipv4.tcp_congestion_control=bbr
net.core.default_qdisc = fq
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_syncookies=1
net.ipv4.ip_local_port_range = 1024 65535
EOF
sysctl -p

echo -e "\033[1mUpdating package lists and installing dependencies\033[0m"
apt-get update
apt-get install -y curl jq

echo -e "\033[1mDownloading geph5-exit\033[0m"
curl -s https://artifacts.geph.io/musl-latest/geph5-exit -o /usr/local/bin/geph5-exit
chmod +x /usr/local/bin/geph5-exit

echo -e "\033[1mCreating configuration directory\033[0m"
mkdir -p /etc/geph5-exit

echo -e "\033[1mGenerating random port numbers\033[0m"
c2e_port=$((10000 + RANDOM % 55535))
b2e_port=$((10000 + RANDOM % 55535))

echo -e "\033[1mGetting country and city using ip-api.com\033[0m"
location_data=$(curl -s 'http://ip-api.com/json/')
country=$(echo "$location_data" | jq -r '.countryCode')
city=$(echo "$location_data" | jq -r '.city')

echo -e "\033[1mGenerating signing secret\033[0m"
dd if=/dev/random of=/etc/geph5-exit/signing.secret bs=1 count=32 2>/dev/null
chmod 600 /etc/geph5-exit/signing.secret

# Perform speed test to calculate total_ratelimit
echo -e "\033[1mPerforming speed test to determine total_ratelimit\033[0m"
speed_test_output=$(curl -w "%{speed_download}" -o /dev/null -s https://artifacts.geph.io/100mb.test)
speed_kps=$(printf "%.0f\n" "$(echo "$speed_test_output / 1024" | bc -l)") # Convert bytes/sec to KB/sec and round

# Check if the configuration file already exists
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
signing_secret: /etc/geph5-exit/signing.secret
total_ratelimit: $speed_kps
EOL
else
  echo -e "\033[1mConfiguration file already exists. Skipping creation.\033[0m"
fi

echo -e "\033[1mCreating systemd service file\033[0m"
cat > /etc/systemd/system/geph5-exit.service <<EOL
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
cat > /etc/systemd/system/geph5-exit-upgrade.timer <<EOL
[Unit]
Description=Geph5 Exit Upgrade Timer

[Timer]
OnCalendar=hourly
Persistent=true

[Install]
WantedBy=timers.target
EOL

echo -e "\033[1mCreating systemd service file for upgrades\033[0m"
cat > /etc/systemd/system/geph5-exit-upgrade.service <<EOL
[Unit]
Description=Geph5 Exit Upgrade Service

[Service]
Type=oneshot
ExecStart=/bin/bash -c 'curl -s https://artifacts.geph.io/musl-latest/geph5-exit -o /tmp/geph5-exit && if ! cmp -s /tmp/geph5-exit /usr/local/bin/geph5-exit; then mv /tmp/geph5-exit /usr/local/bin/geph5-exit && chmod +x /usr/local/bin/geph5-exit && systemctl restart geph5-exit; else rm /tmp/geph5-exit; fi'
EOL

echo -e "\033[1mReloading systemd and starting services\033[0m"
systemctl daemon-reload
systemctl enable --now geph5-exit
systemctl enable --now geph5-exit-upgrade.timer
