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
| `auth_secret_history` | `secret_hash` (PK, 32-byte `bytea`), `user_id` (FK -> `users.id`), `retired_at` | Retired account-code hashes, retained until account deletion. Used for replacement status, retry handling, and preventing code reuse. |
| `auth_password` | `user_id` (PK, FK -> `users.id`), `username`, `pwdhash` | Legacy username/password credentials. Kept for backward compatibility so older clients can still authenticate. Password hashes are stored in PHC format (Argon2). |
| `auth_token_hash` | `token_hash` (PK, 32-byte `bytea`), `user_id` (indexed) | Plain SHA-256 of per-device API tokens minted after authentication. Clients retain and send the original tokens. Preserves the original table's lack of a foreign key; cached validation retains its existing 24-hour TTL. |
| `last_login` | `id` (PK, FK -> `users.id`), `login_time` | Tracks the most recent successful API call per user. Updated on every token validation and used for metrics. |

### Promotions, billing, and bandwidth

| Table | Key columns | Notes |
| --- | --- | --- |
| `free_vouchers` | `id` (PK, FK -> `users.id`), `voucher`, `description`, `visible_after` | Stores promotional gift codes attached to users. If `voucher` is empty the broker will mint one on demand and update the same row. The `description` field contains localized copy as JSON. |
| `plus_periods` | `period_id` (PK), `user_id` (FK -> `users.id`), `start_time`, `end_time`, `tier` | Ledger of paid access windows. Tier `1` corresponds to Geph Plus; tier `0` is Basic. The broker reads this table to count active Plus users and to enforce bandwidth plans. |
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

## Opt-in account-code rotation

The broker creates `auth_secret_history` at startup after the hash cutover, under
the same rollout lock. This step is idempotent and preserves existing account and
token hashes. Registration issues 24-digit codes beginning with `8`. Only legacy codes beginning
with `9` support rotation; `8`-prefixed codes cannot be rotated.
There is no mandatory migration policy or automatic account rotation.

Two new JSON-RPC methods use the normal request-ID response deduplication cache:

* `get_account_secret_status(secret)` returns `Result<AccountSecretStatus, AccountSecretError>`.
  Status is `{"current":{"user_id":123,"invite_code":"..."}}`, `"retired"`, or
  `"invalid"`. `invite_code` is nullable and comes from the stored referral mapping,
  not from the current secret. Retired/invalid statuses contain no account details.
* `rotate_account_secret(current_secret)` returns `Result<String, AccountSecretError>`.
  Success contains the server-generated replacement account code. It accepts a
  legacy `9`-prefixed account code, not a device token or an `8`-prefixed code.

As with other broker `Result` methods, values are wrapped in `{"Ok":...}` or
`{"Err":...}` inside the JSON-RPC result. Errors are `"forbidden"`, `"retired"`,
`"rate_limited"`, or `"unavailable"`. Non-`9`-prefixed inputs return `forbidden`.
Pool acquisition timeouts return `rate_limited`; other database failures return
`unavailable`, never an incorrect-code status.

The broker generates replacements matching `^8[0-9]{23}$`: exactly 24 ASCII digits
beginning with `8`. Existing credentials are interpreted as exact UTF-8 bytes
without trimming or normalization. Replacements are randomly generated and only
their hashes are stored in Postgres for authentication. A separate PostgreSQL
recovery table retains encrypted replacements for 24 hours, keyed by the old
credential hash. ChaCha20-Poly1305 uses a random nonce and a dedicated key derived
from the broker master secret with BLAKE3 domain separation; the old credential
hash is authenticated as associated data. All brokers must share the same master
secret to recover replacements. Changing it makes existing recovery records unreadable.

Registration, rotation, and token issuance use ordinary
`begin()` transactions, relying on the database's default isolation level being
`serializable`. They do not set isolation levels or explicitly acquire locks.
A shared helper retries the entire transaction up to three attempts on serialization
failures, deadlocks, or concurrent uniqueness conflicts; validation errors are not
retried. PostgreSQL's MVCC and uniqueness constraints coordinate credential allocation.

Rotation atomically archives the old hash, installs the replacement hash, and deletes
all device-token rows for that user. User ID, bandwidth accounting,
and stored invite code are preserved. Once an account has rotation history, the
broker also rejects its legacy username/password login, without deleting password
records. Token issuance validates credentials in the same transaction as insertion,
so a conflicting rotation and login cannot both commit an inconsistent result.

For 24 hours after a successful rotation, repeating the call with the old
`9`-prefixed code returns the persisted replacement without changing the account,
deleting newly issued tokens, or granting another subscription reward. Retrieval
works across brokers and restarts and does not extend the deadline. PostgreSQL
sets and checks the expiry; expired records are removed by the database GC loop.
After expiry the API returns `retired`. Rotation and recovery storage commit in
the same transaction. Rotations completed before this storage was deployed cannot
be recovered from their hashes.

The old code is immediately retired for ordinary authentication. For the first
12 hours of the 24-hour recovery period, `get_account_secret_status` reports it
as `current` with the account's user ID and invite code so existing GUIs offer
the normal upgrade screen on additional devices. After 12 hours it reports
`retired`, while the rotation API still permits recovery until 24 hours. Old
rotations without recovery records report `retired` immediately. This status
does not authorize login or token issuance. Anyone
holding the old code, including an attacker, can retrieve the replacement through
that API during these 24 hours.

The client must save the returned code, then obtain a fresh device token using
`get_auth_token`. Concurrent rotations use transaction retries to retrieve the
winning replacement from PostgreSQL.
Clients use fresh request IDs for new operations and reuse IDs for transport retries.
The existing response deduplication cache can replay an earlier success for its
normal 120-second lifetime, including just beyond the grace-period deadline.

All broker instances must run this version before exposing rotation; older instances
do not authenticate and issue tokens in one transaction. The existing 24-hour
device-token validation cache is intentionally unchanged: deleted tokens may remain
usable until their cached entries expire or all processes holding them restart.
Restarting after each rotation is not required. In-flight authenticated operations
may finish. Rotation does not terminate established tunnels, invalidate previously
issued anonymous connection/bandwidth tokens, revoke billing sessions, or change
the payment service's legacy-password policy. Account deletion remains disabled.

This is a first-claimant-wins recovery mechanism; it does not identify the legitimate
owner of leaked credentials. Client UI, payments integration, and rollout activation
are separate work.

## Secret-rotation subscription reward

A successful rotation by an active Plus or Basic user grants seven days (168 hours)
of their highest currently active tier: tier-1 Plus or tier-0 Basic. Plus rewards
start after the latest Plus period; Basic rewards start after all paid periods,
matching billing and avoiding overlap with scheduled Plus. The reward and rotation
commit atomically. Expired, free, and future-only paid accounts do not qualify. Existing subscription caches pick up the new ledger period normally.

At startup, after creating secret history, the broker creates
`auth_secret_rotation_rewards` and processes previously rotated accounts. Users
with active Plus or Basic at backfill time receive the same seven-day extension. Each
account is recorded once, including ineligible accounts, so retries, restarts,
and later purchases cannot award additional rotation bonuses. A failed backfill
rolls back and prevents startup; restarting retries it. The reward uses
`plus_periods`, without creating payment revenue or changing recurring billing.

Accounts marked ineligible by the original Plus-only rollout are reconsidered if
they had active Basic at their recorded `processed_at` time and currently have
active Plus or Basic. Already granted rewards remain unchanged. Accounts that
were free at their original evaluation do not become eligible by purchasing later.
