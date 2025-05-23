# geph5-broker

`geph5-broker` is the central coordination service.  Clients contact the broker to authenticate, obtain connect tokens and receive a list of available exits.  Bridges and exits also register themselves here.  The broker exposes a JSON‑RPC API over HTTP and a raw TCP protocol for bridge/exit communication.

Configuration is provided via YAML as parsed by `ConfigFile` in `src/main.rs`.  Paths to cryptographic keys, database connection information and listener addresses are specified here.

Example configuration:

```yaml
listen: 0.0.0.0:9000
tcp_listen: 0.0.0.0:9001
master_secret: /etc/geph5/broker/master.bin
mizaru_keys: /etc/geph5/broker/mizaru
postgres_url: postgres://geph:password@localhost/geph
bridge_token: bridge_secret
exit_token: exit_secret
puzzle_difficulty: 24
payment_url: https://web-backend.geph.io/rpc
payment_support_secret: support-secret
```
