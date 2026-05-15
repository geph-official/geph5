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
free_port_whitelist: [20,21,43,53,79,80,81,88,110,143,220,389,443,464,531,543,544,554,636,706,749,873,902,903,904,981,989,990,991,992,993,995,1194,1220,1293,1500,1533,1677,1723,1755,1863,2083,2086,2087,2095,2096,2102,2103,2104,3690,4321,4643,5050,5190,5222,5223,5228,8008,8074,8082,8087,8088,8332,8333,8443,8888,9418,10000,11371,19294,19638,50002,64738]
task_limit: 1000000
ipv6_subnet: "2001:db8::/64"
ipv6_pool_size: 100
```
