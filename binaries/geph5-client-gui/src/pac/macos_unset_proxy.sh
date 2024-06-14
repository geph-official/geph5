#!/bin/bash

# Get the list of network services
network_services=$(networksetup -listallnetworkservices | tail -n +2)

# Iterate over each network service and disable the proxy
for service in $network_services; do
    # Disable the HTTP proxy
    networksetup -setwebproxystate "$service" off

    # Disable the HTTPS proxy
    networksetup -setsecurewebproxystate "$service" off
done
