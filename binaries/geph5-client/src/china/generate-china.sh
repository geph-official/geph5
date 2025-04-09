#!/usr/bin/env bash

curl -s "https://raw.githubusercontent.com/felixonmars/dnsmasq-china-list/master/accelerated-domains.china.conf" \
  | awk -F'/' '{print $2}'
