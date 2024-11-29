export PATH=$PATH:/usr/sbin/:/sbin/

# Clear IPv4 table
ip route flush table 8964
ip route add default dev tun-geph table 8964

# Clear IPv6 table (create it if it doesn't exist)
ip -6 route flush table 8964
ip -6 route add default dev tun-geph table 8964

# Set up rules for IPv4
ip rule add table main suppress_prefixlength 0
ip rule add lookup 8964 pref 2

# Set up rules for IPv6
ip -6 rule add table main suppress_prefixlength 0
ip -6 rule add lookup 8964 pref 2

# Redirect DNS requests for IPv4
iptables -t nat -A OUTPUT -p udp --dport 53 -j DNAT --to $GEPH_DNS
iptables -t nat -A OUTPUT -p tcp --dport 53 -j DNAT --to $GEPH_DNS

# Redirect DNS requests for IPv6
#ip6tables -t nat -A OUTPUT -p udp --dport 53 -j DNAT --to $GEPH_DNS_IPV6
#ip6tables -t nat -A OUTPUT -p tcp --dport 53 -j DNAT --to $GEPH_DNS_IPV6
