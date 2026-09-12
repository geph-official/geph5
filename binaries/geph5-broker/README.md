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
| `auth_secret_hash` | `id` (PK, FK -> `users.id`), `secret_hash` (unique, 32-byte `bytea`) | Stores plain SHA-256 of the exact UTF-8 login secret, without a prefix, salt, or stretching. Registration also stores the existing client-compatible referral code. |
| `auth_password` | `user_id` (PK, FK -> `users.id`), `username`, `pwdhash` | Legacy username/password credentials. Kept for backward compatibility so older clients can still authenticate. Password hashes are stored in PHC format (Argon2). |
| `auth_token_hash` | `token_hash` (PK, 32-byte `bytea`), `user_id` (indexed) | Plain SHA-256 of per-device API tokens minted after authentication. Clients retain and send the original tokens. Preserves the original table's lack of a foreign key; cached validation retains its existing 24-hour TTL. |
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
| `bridges_new` | `listen` (PK), `cookie`, `pool`, `expiry` | Live registry of bridge front-ends. `listen` is the control address, `cookie` is the enrollment secret, `pool` groups bridges by location/provider pool, and `expiry` is stored as a Unix timestamp. Expired rows are purged by the GC loop. |
| `bridge_pool_delays` | `pool` (PK), `delay_ms`, `is_plus` | SLO overrides for entire bridge pool prefixes, written by bridge-phalanx. `delay_ms` inflates the perceived latency for overloaded pools and `is_plus` marks pools that should only be served to Plus users. The broker applies the longest prefix whose `pool` matches the registered bridge pool name, so a row like `foo` also covers `foo_ipv6` unless a more specific prefix exists. |
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

## Account and device-secret hash cutover

The account-secret cutover requires a coordinated restart of the broker and
`geph-payments-2`. The later device-token cutover requires stopping old brokers
and other token writers; payments does not read or write these tokens.
The broker embeds the SQL file pairs in `sql/` and executes them directly at startup,
before listeners or background jobs start. No SQLx migration framework or history
table is used. Stop the old broker and payments service and pause manual account
changes, then start the updated broker. Once it reports that the cutover committed
and starts serving, start the updated payments binary. The GUI no longer offers legacy
conversion; the broker rejects legacy conversion while retaining ordinary legacy
username/password authentication.

Each file owns its transaction; the broker does not wrap them in another transaction.
Within each pair, the first creates the empty table and indexes, copies the original
table's grants (including support access), and commits before the backfill.
The account-secret backfill verifies the hashes, preserves existing referral codes,
fills missing codes, updates the three `*bysecret` support functions, and drops the plaintext
table using `RESTRICT`. It preserves user IDs, passwords, and subscriptions.

The `auth_token_hash_01_create.sql` / `auth_token_hash_02_backfill.sql` pair then
copies every per-device token as SHA-256 of its exact UTF-8 bytes, without a prefix,
salt, or stretching. It writes each token-to-user mapping directly, overwriting any
conflicting destination mapping, then drops `auth_tokens` using `RESTRICT` without
a separate verification scan. Existing clients and stored device tokens continue to work without
reauthentication. Issuance stores only hashes; lookups and their cache keys also use
hashes. No client or payment-service change is needed for this token migration.
Databases that already completed the account-secret cutover run only the token pair.

All scripts limit lock waits to 250 ms. Schema statements have a two-second
execution timeout; bulk statements in each backfill have five minutes each.
A failed script rolls back on disconnect and startup fails before serving requests.
If a backfill fails, its create script stays committed; restarting the broker retries
that backfill after the error is resolved. Its source table remains intact until
it succeeds. Each pair commits independently: if the token pair
fails after account migration, the account migration remains committed. Subsequent
starts skip each pair once its plaintext table is gone and its hash table exists.
A dedicated connection holds a nonblocking advisory lock across all files to prevent
concurrent brokers from running the rollout; another broker attempting startup during
the rollout exits with a retry instruction.
The connection closes on success or failure, releasing the lock. Old writers do not
participate in this lock and must be stopped. After success, old binaries cannot be used.

Validate the cutover with existing-device tokens, existing-account login, new registration,
website login, referral codes, and support grants. The registration transaction stores the hash
and referral code together, so no periodic plaintext-reading referral job is needed.

For local verification, set `GEPH_TEST_DATABASE_URL` to disposable PostgreSQL on
loopback. Run `cargo test -p geph5-broker -- --include-ignored` from the workspace
root, and `python3 tests/test_secret_hash_cutover.py` from this directory. Set
`GEPH_HASH_VOLUME_TEST=1` for the optional 3.55-million-account backfill test.
Set `GEPH_TOKEN_HASH_VOLUME_TEST=1` for the optional 12-million-token backfill test.
The SQL and startup-rollout tests create and remove their own databases; other broker
database tests use temporary tables. All reject non-loopback database hosts.
