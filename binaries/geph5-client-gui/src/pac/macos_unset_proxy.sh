#!/bin/bash

# AppleScript to request administrator privileges and run the script
osascript -e 'do shell script "
# Get the list of network services
network_services=$(networksetup -listallnetworkservices | tail -n +2)

# Loop through each line using read
while IFS= read -r service; do
  # Disable HTTP and HTTPS proxies
  networksetup -setwebproxystate \"$service\" off
  networksetup -setsecurewebproxystate \"$service\" off
done <<< \"$network_services\"
" with administrator privileges'
