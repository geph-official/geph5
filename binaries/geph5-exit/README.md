# geph5-exit

`geph5-exit` is the server component that terminates client tunnels and forwards traffic to the open Internet.  Exits register with the broker so that clients can discover them.  Each exit enforces rate limits and may restrict access based on account level or country.

Configuration is supplied in YAML following the `ConfigFile` struct in `src/main.rs`.  The file specifies keys used to sign descriptors, how the exit connects to the broker and networking details such as listening addresses and country information.

Example configuration:

```yaml
signing_secret: /etc/geph5/exit/signing.key
broker:
  url: https://broker.geph.io/
  auth_token: my-secret
c2e_listen: 0.0.0.0:9002
b2e_listen: 0.0.0.0:9003
ip_addr: 203.0.113.5
country: USA
city: NYC
metadata:
  allowed_levels: [plus]
  category: [core]
country_blacklist: []
free_ratelimit: 300
plus_ratelimit: 30000
total_ratelimit: 125000
free_port_whitelist: [80,443,8080,8443,22,53]
task_limit: 1000000
ipv6_subnet: "2001:db8::/64"
ipv6_pool_size: 100
```
