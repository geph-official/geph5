# The Geph5 stats system

All metrics live in the same Postgres database as the rest of the broker data.
There is exactly one way stats flow, one place they are stored, and one
datasource behind every Grafana panel. (InfluxDB and the old ad-hoc paths are
deprecated; see [Decommissioning](#decommissioning-influxdb) below.)

```
bridges ──(report_stats RPC, bridge_token MAC, 10s batches)──┐
exits ────(report_stats RPC, exit auth_token MAC, ~2s loop)──┤
broker's own metrics (RPC timers, self-stat gauges) ─────────┴─► broker
        broker ──DogStatsD UDP, localhost:8125──► Telegraf ──batched SQL──► Postgres
                                                                              │
                                              Grafana ◄── metric() / rollups ─┘
```

Design goals, in order:

1. **statsd ergonomics**: emitting a stat is one fire-and-forget call. No
   stat can ever block, slow down, or take down the caller.
2. **Tagged, high-cardinality data**: every stat carries named tags
   (`pool=viet asn=4134 country=CN`), not positional dot-paths. "Top ASNs by
   traffic" is a `GROUP BY`, and metrics can be JOINed against business
   tables (users, subscriptions) because they're all in the same database.
3. **Whisper-style resolution transparency**: dashboards query `metric(...)`
   and never care that storage has raw/minutely/hourly tiers.
4. **Static plumbing**: adding a new metric requires *zero* configuration —
   no Telegraf rules, no schema migration, no Grafana datasource work.

## Emitting stats

### From the broker

The broker emits directly to the local Telegraf via UDP
(`STATS_SINK` in `binaries/geph5-broker/src/rpc_impl.rs`, address from the
`statsd_addr` config field):

```rust
use geph5_stats::StatEvent;

if let Some(sink) = STATS_SINK.as_ref() {
    sink.send_one(&StatEvent::timer_ms("broker_rpc_calls", &[("method", method)], ms));
    sink.send_many(&events); // batches into as few UDP datagrams as possible
}
```

### From bridges and exits

Semi-trusted fleet nodes never talk to Telegraf or Postgres. They batch
locally and ship to the broker over the **`report_stats`** RPC, authenticated
with the same MAC scheme as `insert_bridge`/`insert_exit`:

```rust
// bridges: accumulate into the global batcher (see geph5-bridge/src/stats.rs);
// a background loop drains it to the broker every 10s.
STAT_BATCHER.counter("bridge_bytes", &[("pool", pool), ("asn", asn), ("country", cc)], bytes);

// exits: send a batch of gauges from the existing 2s broker loop.
client.report_stats(Mac::new(stats, blake3::hash(auth_token.as_bytes()).as_bytes())).await?;
```

The broker verifies the MAC against `bridge_token` or `exit_token` and
re-emits the events to its local statsd sink — so fleet stats and broker
stats take literally the same path into storage.

### Event semantics (`libraries/geph5-stats`)

| kind | statsd type | Telegraf aggregation per 10s window |
|---|---|---|
| `counter` | `\|c` | sum |
| `gauge` | `\|g` | last value |
| `timer_ms` | `\|ms` | count / mean / stddev / upper / lower / p90 / p99 |

Timers are the cheap way to get rates **and** latencies: one
`timer_ms("broker_rpc_calls", ...)` per request yields RPS
(`count / 10`) and the whole latency distribution, with no query-side work.

`StatBatcher` (used by fleet nodes) pre-aggregates between flushes: counters
with identical name+tags are summed, gauges keep the last value, timers are
kept individually.

## Transport: Telegraf

Config lives at `binaries/geph5-broker/telegraf/telegraf.conf` and is
deployed on the broker host at `/etc/telegraf/telegraf.conf` (the deployed
copy carries the real Postgres credentials; the repo copy has placeholders).

- `inputs.statsd` with `datadog_extensions = true`: tags travel **inside**
  each packet (`bridge_bytes:1048576|c|#pool:viet,asn:4134,country:CN`), so
  the config never grows per-metric rules. The `templates` block that parses
  old dot-path stats is legacy and dies with the last old exit.
- `outputs.postgresql`: auto-creates one table per measurement and new
  columns for new tags. `metric_buffer_limit = 100000` rides out Aiven
  failovers.
- The listener binds `127.0.0.1` only — nothing off-host can spoof stats.

**Adding a new metric end-to-end**: call
`stats.counter("my_new_thing", &[("color", "red")], 1.0)` somewhere. That's
it. Telegraf creates the `my_new_thing` table on first flush. (For rollups
and `metric()` support, add matching `_minutely`/`_hourly` tables and a
clause in `metrics_rollup_tier()` — see `sql/metrics.sql`.)

## Storage: Postgres resolution tiers

Schema: `binaries/geph5-broker/sql/metrics.sql` — idempotent, apply with
`psql "$POSTGRES_URL" -f metrics.sql`.

| tier | resolution | retention | example |
|---|---|---|---|
| raw | ~10s (Telegraf flush) | 14 days | `bridge_bytes` |
| minutely | 1 min | 90 days | `bridge_bytes_minutely` |
| hourly | 1 h | forever | `bridge_bytes_hourly` |

Maintained by pg_cron:

- `geph5-metrics-rollup` (every 10 min): upserts the trailing 3 hours of raw
  into both rollup tiers, so the current partial bucket keeps refreshing and
  late data is absorbed.
- `geph5-metrics-retention` (daily 04:20): deletes raw > 14 d and
  minutely > 90 d.

Rollup aggregation: gauges → mean, counters → sum, timers → count-weighted
mean + worst-case upper/p99.

## Querying: the `metric()` function

```sql
metric(name text, from timestamptz, to timestamptz, bucket interval,
       agg text DEFAULT 'avg',     -- avg | sum | max | min
       field text DEFAULT 'value') -- e.g. 'mean'/'count' for timer tables
  RETURNS TABLE ("time" timestamptz, tags jsonb, value double precision)
```

It picks and **unions** tiers by bucket size and actual coverage — Whisper
semantics, rebuilt in SQL:

- bucket ≥ 1 h → hourly only
- 1 min ≤ bucket < 1 h → minutely where it exists, raw for the
  not-yet-rolled-up tail, hourly for older history
- bucket < 1 min → raw wherever raw exists, then minutely, then hourly

Typical Grafana panel queries (Postgres datasource):

```sql
-- per-exit throughput (kbps is kilobytes/s, ×8000 = bits/s)
SELECT "time", tags->>'exit' AS exit, value * 8000 AS bps
FROM metric('kbps', $__timeFrom(), $__timeTo(), interval '$__interval')
ORDER BY 1;

-- bridged traffic by pool, as bits/s
SELECT "time", tags->>'pool' AS pool,
       sum(value) * 8 / extract(epoch from interval '$__interval') AS bps
FROM metric('bridge_bytes', $__timeFrom(), $__timeTo(), interval '$__interval', 'sum')
GROUP BY 1, 2 ORDER BY 1;

-- RPC rate and latency from the timer table
SELECT "time", tags->>'method' AS method,
       value / extract(epoch from interval '$__interval') AS rps
FROM metric('broker_rpc_calls', $__timeFrom(), $__timeTo(), interval '$__interval', 'sum', 'count')
ORDER BY 1;
```

Whole-range aggregations (pie charts) can hit `*_hourly` directly; with
table-format SQL, set the pie panel's `reduceOptions` to "All values".

## Measurements

| table | tags | written by | meaning |
|---|---|---|---|
| `kbps`, `load`, `uptime`, `task_count`, `schedlag` | exit | exits | per-exit gauges; `kbps` is kilobytes/s |
| `bridge_bytes` | pool, asn, country | bridges | bridged traffic counter, bytes |
| `broker_rpc_calls` | method | broker | per-RPC timer: count/mean/p90/p99 per 10s, milliseconds |
| `bridge_pools` | pool | broker self-stat | bridge count per pool |
| `broker_logins` | kind = daily \| daily_new \| weekly | broker self-stat | login counts |
| `plus` | — | broker self-stat | subscription count |
| `broker_sysstat` | ip_addr | broker self-stat | normalized load factor |

## Decommissioning InfluxDB

InfluxDB history (since 2025-06) was backfilled into the hourly tier, and
the most recent 30 days into the minutely tier (where backfilled
`bridge_bytes_minutely` rows have `asn = ''`; per-ASN history is
hourly-only). During the transition Telegraf dual-writes to InfluxDB and
old fleet members keep working through deprecated shims:

- `set_stat`/`incr_stat` RPCs (unauthenticated, legacy) are forwarded by the
  broker as classic dot-path statsd lines, parsed by the legacy `templates`
  in telegraf.conf.
- Old bridges still write `bridge_bytes` straight to InfluxDB; that data
  stops being visible in Grafana once the panels read Postgres, so re-run
  the backfill after the bridge fleet upgrades to close the gap.

Once the whole fleet runs the new code and dashboards look right:

1. Remove the `templates` block and `[[outputs.influxdb]]` from
   telegraf.conf; redeploy.
2. Remove the `set_stat`/`incr_stat` shims from broker + protocol.
3. `systemctl disable --now influxdb` on the broker host; reclaim ~290 GB
   in `/var/lib/influxdb`.
4. Delete the InfluxDB datasource from Grafana.
5. Drop the unused `influxdb:` block from `/etc/geph5-broker/config.yaml`.
