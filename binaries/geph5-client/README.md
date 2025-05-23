# geph5-client

`geph5-client` is the main user facing daemon. It establishes and maintains a session with the broker and chosen exits, then exposes local proxy endpoints (SOCKS5/HTTP) to tunnel user traffic through the Geph network.  The binary can optionally operate in VPN mode by tunnelling packets directly.  It also exposes a control RPC interface used by GUI front‑ends.

The client is configured with a YAML file matching the `Config` struct in `src/client.rs`.  Most fields are optional and control which services listen locally, how to reach the broker, and optional VPN or bridge settings.

Example configuration:

```yaml
socks5_listen: 127.0.0.1:9910
http_proxy_listen: 127.0.0.1:9911
pac_listen: 127.0.0.1:9912
control_listen: 127.0.0.1:9913
exit_constraint: auto
bridge_mode: auto
cache: /var/cache/geph5-client
broker:
  direct: "https://broker.geph.io/"
broker_keys:
  master: "deadbeef..."
  mizaru_free: "deadbeef..."
  mizaru_plus: "deadbeef..."
vpn: false
spoof_dns: false
passthrough_china: false
credentials:
  legacy_username_password:
    username: "user"
    password: "pass"
```
