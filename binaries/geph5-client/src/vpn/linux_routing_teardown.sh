#!/bin/bash

# Add /usr/sbin/ and /sbin to the PATH in case they are not already available
export PATH=$PATH:/usr/sbin/:/sbin/

# Clear IPv4 table 8964
ip route flush table 8964

# Clear IPv6 table 8964
ip -6 route flush table 8964

# Remove rules for IPv4
ip rule del table main suppress_prefixlength 0 || echo "No IPv4 suppress rule found"
ip rule del lookup 8964 pref 2 || echo "No IPv4 rule for table 8964 found"

# Remove rules for IPv6
ip -6 rule del table main suppress_prefixlength 0 || echo "No IPv6 suppress rule found"
ip -6 rule del lookup 8964 pref 2 || echo "No IPv6 rule for table 8964 found"

# Remove redirection of DNS requests for IPv4
iptables -t nat -D OUTPUT -p udp --dport 53 -j DNAT --to $GEPH_DNS || echo "No IPv4 UDP DNS redirection rule found"
iptables -t nat -D OUTPUT -p tcp --dport 53 -j DNAT --to $GEPH_DNS || echo "No IPv4 TCP DNS redirection rule found"

# Remove redirection of DNS requests for IPv6
if [ -n "$GEPH_DNS_IPV6" ]; then
  ip6tables -t nat -D OUTPUT -p udp --dport 53 -j DNAT --to $GEPH_DNS_IPV6 || echo "No IPv6 UDP DNS redirection rule found"
  ip6tables -t nat -D OUTPUT -p tcp --dport 53 -j DNAT --to $GEPH_DNS_IPV6 || echo "No IPv6 TCP DNS redirection rule found"
fi

# Remove the IPv6 address from the TUN interface
ip -6 addr del fd64:8964::1/64 dev tun-geph || echo "No IPv6 address on tun-geph"

echo "Script execution complete. Reverse actions applied."
