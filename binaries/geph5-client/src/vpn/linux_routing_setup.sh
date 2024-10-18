export PATH=$PATH:/usr/sbin/:/sbin/
ip route flush table 8964
ip route add default dev tun-geph table 8964

ip rule del table main suppress_prefixlength 0
ip rule add table main suppress_prefixlength 0
ip rule del lookup 8964 pref 2
ip rule add lookup 8964 pref 2
iptables -t nat -D OUTPUT -p udp --dport 53 -j DNAT --to $GEPH_DNS
iptables -t nat -D OUTPUT -p tcp --dport 53 -j DNAT --to $GEPH_DNS
iptables -t nat -A OUTPUT -p udp --dport 53 -j DNAT --to $GEPH_DNS
iptables -t nat -A OUTPUT -p tcp --dport 53 -j DNAT --to $GEPH_DNS

# block ipv6 completely
ip6tables -D OUTPUT -o lo -j ACCEPT
ip6tables -A OUTPUT -o lo -j ACCEPT
ip6tables -D OUTPUT  -j REJECT
ip6tables -A OUTPUT  -j REJECT