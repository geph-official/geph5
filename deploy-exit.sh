#!/bin/bash

if [ -z "$AUTH_TOKEN" ]; then
  read -p "Enter the auth_token: " auth_token
else
  auth_token="$AUTH_TOKEN"
fi

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
EOL

echo -e "\033[1mCreating systemd service file\033[0m"
cat > /etc/systemd/system/geph5-exit.service <<EOL
[Unit]
Description=Geph5 Exit
After=network.target

[Service]
ExecStart=/usr/local/bin/geph5-exit --config /etc/geph5-exit/config.yaml
Restart=always

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
