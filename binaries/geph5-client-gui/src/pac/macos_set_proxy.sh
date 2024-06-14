#!/bin/bash


# Get the list of network services
network_services=$(networksetup -listallnetworkservices | tail -n +2)

# Iterate over each network service and set the proxy
for service in $network_services; do
    # Set the HTTP proxy
    networksetup -setwebproxy "$service" $proxy_server $port
    networksetup -setwebproxystate "$service" on

    # Set the HTTPS proxy
    networksetup -setsecurewebproxy "$service" $proxy_server $port
    networksetup -setsecurewebproxystate "$service" on
done
