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

## PostgreSQL tables

### Authentication and identity

| Table | Key columns | Notes |
| --- | --- | --- |
| `users` | `id` (PK), `createtime` | Canonical user record. A row is created whenever a new secret account is issued. Other tables reference this `id`. |
| `auth_secret` | `id` (PK, FK -> `users.id`), `secret` | Stores the numeric login secrets issued to users. Secrets are rotated by overwriting the existing row. |
| `auth_password` | `user_id` (PK, FK -> `users.id`), `username`, `pwdhash` | Legacy username/password credentials. Kept for backward compatibility so older clients can still authenticate. Password hashes are stored in PHC format (Argon2). |
| `auth_tokens` | `token` (PK), `user_id` (FK -> `users.id`) | API tokens minted by the broker after successful authentication. Tokens remain valid until the row is deleted (for example via account removal). |
| `last_login` | `id` (PK, FK -> `users.id`), `login_time` | Tracks the most recent successful API call per user. Updated on every token validation and used for metrics. |

### Promotions, billing, and bandwidth

| Table | Key columns | Notes |
| --- | --- | --- |
| `free_vouchers` | `id` (PK, FK -> `users.id`), `voucher`, `description`, `visible_after` | Stores promotional gift codes attached to users. If `voucher` is empty the broker will mint one on demand and update the same row. The `description` field contains localized copy as JSON. |
| `plus_periods` | `period_id` (PK), `user_id` (FK -> `users.id`), `start_time`, `end_time`, `tier` | Ledger of paid access windows. Tier `0` corresponds to Geph Plus. The broker reads this table to count active Plus users and to enforce bandwidth plans. |
| `subscriptions` | `id` (PK, FK -> `users.id`), `plan`, `expires`, ... | Current active subscription for each user. The broker only needs the expiry timestamp but other billing services may add additional bookkeeping columns. |
| `stripe_recurring` | `subscription_id` (PK), `user_id` (FK -> `users.id`) | Links Stripe subscription IDs to Geph user IDs so recurring billing can be cancelled or migrated. Joined with `subscriptions` to detect recurring customers. |
| `bw_limits` | `id` (PK, FK -> `users.id`), `mb_limit`, `renew_mb`, `renew_date` | Optional per-user bandwidth caps. The broker reads the remaining quota and reset schedule when accounting traffic. |
| `bw_usage` | `id` (PK, FK -> `users.id`), `mb_used` | Aggregated downstream usage in MiB. Updated atomically whenever the exit reports consumption. |

### Bridge registry and telemetry

| Table | Key columns | Notes |
| --- | --- | --- |
| `bridges_new` | `listen` (PK), `cookie`, `pool`, `expiry` | Live registry of bridge front-ends. `listen` is the control address, `cookie` is the enrollment secret, `pool` groups bridges by allocation group, and `expiry` is stored as a Unix timestamp. Expired rows are purged by the GC loop. |
| `bridge_group_delays` | `pool` (PK), `delay_ms`, `is_plus` | SLO overrides for entire bridge pools, written by bridge-phalanx. `delay_ms` inflates the perceived latency for overloaded pools and `is_plus` marks pools that should only be served to Plus users. |
| `bridge_availability` | `listen`, `user_country`, `user_asn`, `successes`, `failures`, `last_update` | Per-(bridge, country, ASN) success counters with exponential decay. Used to bias bridge selection toward routes that work for a user’s location. |

### Exit registry

| Table | Key columns | Notes |
| --- | --- | --- |
| `exits_new` | `pubkey` (PK), `c2e_listen`, `b2e_listen`, `country`, `city`, `load`, `expiry` | Active exits advertised to clients. Contains the control and bridge endpoints, location metadata, current load factor, and a Unix timestamp expiry. Stale rows are purged by the GC loop. |
| `exit_metadata` | `pubkey` (PK, FK -> `exits_new.pubkey`), `metadata` (JSONB) | Optional rich metadata pushed by operators (e.g., feature flags). Served alongside exit descriptors when present. |

### Abuse prevention

| Table | Key columns | Notes |
| --- | --- | --- |
| `used_puzzles` | `puzzle` (PK) | Deduplicates proof-of-work puzzles solved by clients. Inserting a puzzle that already exists causes the request to be rejected. |
