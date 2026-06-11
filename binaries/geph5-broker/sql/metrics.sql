-- Geph5 metrics schema: raw tables fed by Telegraf (statsd -> outputs.postgresql),
-- Whisper-style resolution tiers maintained by pg_cron, and a metric() helper
-- that mixes resolutions so Grafana panels never have to name rollup tables.
--
--   raw      ~10s points   kept 14 days
--   minutely 1-min rollup  kept 90 days
--   hourly   1-h rollup    kept forever
--
-- Apply with: psql "$POSTGRES_URL" -f metrics.sql
-- Idempotent: safe to re-run.

------------------------------------------------------------------------------
-- Raw tables. Telegraf auto-creates missing tables/columns, but we pre-create
-- them to control types and indexes. One table per measurement; statsd tags
-- become text columns, the value lands in "value" (gauges/counters) or in the
-- aggregate fields Telegraf computes for timers.
------------------------------------------------------------------------------

-- Exit gauges, tagged by exit (reported every ~2s per exit via broker RPC).
CREATE TABLE IF NOT EXISTS "kbps"       ("time" timestamptz NOT NULL, "exit" text, "value" double precision);
CREATE TABLE IF NOT EXISTS "load"       ("time" timestamptz NOT NULL, "exit" text, "value" double precision);
CREATE TABLE IF NOT EXISTS "uptime"     ("time" timestamptz NOT NULL, "exit" text, "value" double precision);
CREATE TABLE IF NOT EXISTS "task_count" ("time" timestamptz NOT NULL, "exit" text, "value" double precision);
CREATE TABLE IF NOT EXISTS "schedlag"   ("time" timestamptz NOT NULL, "exit" text, "value" double precision);

-- Bridge traffic counter, tagged by pool/asn/country (summed per 10s flush window).
CREATE TABLE IF NOT EXISTS "bridge_bytes"
    ("time" timestamptz NOT NULL, "pool" text, "asn" text, "country" text, "value" double precision);

-- Broker self-stats (30s loop).
CREATE TABLE IF NOT EXISTS "bridge_pools"  ("time" timestamptz NOT NULL, "pool" text, "value" double precision);
CREATE TABLE IF NOT EXISTS "broker_logins" ("time" timestamptz NOT NULL, "kind" text, "value" double precision);
CREATE TABLE IF NOT EXISTS "plus"          ("time" timestamptz NOT NULL, "value" double precision);
CREATE TABLE IF NOT EXISTS "broker_sysstat" ("time" timestamptz NOT NULL, "ip_addr" text, "value" double precision);

-- Per-RPC timer; Telegraf's statsd input aggregates each 10s window into
-- count/mean/percentiles, so row volume is per-method, not per-call.
CREATE TABLE IF NOT EXISTS "broker_rpc_calls" (
    "time" timestamptz NOT NULL,
    "method" text,
    "mean" double precision,
    "stddev" double precision,
    "sum" double precision,
    "upper" double precision,
    "lower" double precision,
    "count" bigint,
    "90_percentile" double precision,
    "99_percentile" double precision
);

CREATE INDEX IF NOT EXISTS kbps_time_idx             ON "kbps" ("time");
CREATE INDEX IF NOT EXISTS load_time_idx             ON "load" ("time");
CREATE INDEX IF NOT EXISTS uptime_time_idx           ON "uptime" ("time");
CREATE INDEX IF NOT EXISTS task_count_time_idx       ON "task_count" ("time");
CREATE INDEX IF NOT EXISTS schedlag_time_idx         ON "schedlag" ("time");
CREATE INDEX IF NOT EXISTS bridge_bytes_time_idx     ON "bridge_bytes" ("time");
CREATE INDEX IF NOT EXISTS bridge_bytes_pool_time_idx ON "bridge_bytes" ("pool", "time");
CREATE INDEX IF NOT EXISTS bridge_pools_time_idx     ON "bridge_pools" ("time");
CREATE INDEX IF NOT EXISTS broker_logins_time_idx    ON "broker_logins" ("time");
CREATE INDEX IF NOT EXISTS plus_time_idx             ON "plus" ("time");
CREATE INDEX IF NOT EXISTS broker_sysstat_time_idx   ON "broker_sysstat" ("time");
CREATE INDEX IF NOT EXISTS broker_rpc_calls_time_idx ON "broker_rpc_calls" ("time");

------------------------------------------------------------------------------
-- Hourly rollups. Raw data is kept RAW_RETENTION (14 days); hourly forever.
-- Tag columns default to '' so they can participate in primary keys.
------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS "kbps_hourly"
    ("time" timestamptz NOT NULL, "exit" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "exit"));
CREATE TABLE IF NOT EXISTS "load_hourly"
    ("time" timestamptz NOT NULL, "exit" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "exit"));
CREATE TABLE IF NOT EXISTS "uptime_hourly"
    ("time" timestamptz NOT NULL, "exit" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "exit"));
CREATE TABLE IF NOT EXISTS "task_count_hourly"
    ("time" timestamptz NOT NULL, "exit" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "exit"));
CREATE TABLE IF NOT EXISTS "schedlag_hourly"
    ("time" timestamptz NOT NULL, "exit" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "exit"));
CREATE TABLE IF NOT EXISTS "bridge_bytes_hourly"
    ("time" timestamptz NOT NULL,
     "pool" text NOT NULL DEFAULT '', "asn" text NOT NULL DEFAULT '', "country" text NOT NULL DEFAULT '',
     "value" double precision,
     PRIMARY KEY ("time", "pool", "asn", "country"));
CREATE TABLE IF NOT EXISTS "bridge_pools_hourly"
    ("time" timestamptz NOT NULL, "pool" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "pool"));
CREATE TABLE IF NOT EXISTS "broker_logins_hourly"
    ("time" timestamptz NOT NULL, "kind" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "kind"));
CREATE TABLE IF NOT EXISTS "plus_hourly"
    ("time" timestamptz NOT NULL, "value" double precision,
     PRIMARY KEY ("time"));
CREATE TABLE IF NOT EXISTS "broker_sysstat_hourly"
    ("time" timestamptz NOT NULL, "ip_addr" text NOT NULL DEFAULT '', "value" double precision,
     PRIMARY KEY ("time", "ip_addr"));
CREATE TABLE IF NOT EXISTS "broker_rpc_calls_hourly"
    ("time" timestamptz NOT NULL, "method" text NOT NULL DEFAULT '',
     "count" double precision, "mean" double precision, "upper" double precision,
     "p99" double precision,
     PRIMARY KEY ("time", "method"));

------------------------------------------------------------------------------
-- Minutely rollups: same shape (including the primary key) as the hourly ones.
------------------------------------------------------------------------------

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'kbps', 'load', 'uptime', 'task_count', 'schedlag', 'bridge_bytes',
        'bridge_pools', 'broker_logins', 'plus', 'broker_sysstat', 'broker_rpc_calls'
    ] LOOP
        EXECUTE format('CREATE TABLE IF NOT EXISTS %I (LIKE %I INCLUDING ALL)',
                       t || '_minutely', t || '_hourly');
    END LOOP;
END $$;

------------------------------------------------------------------------------
-- Rollup job: upserts the trailing window into both tiers, so the current
-- partial bucket keeps refreshing and late data is absorbed. Idempotent.
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION metrics_rollup_tier(trunc_unit text, suffix text, cutoff timestamptz)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    gauge_table text;
BEGIN
    -- Gauges tagged by exit: mean per bucket.
    FOREACH gauge_table IN ARRAY ARRAY['kbps', 'load', 'uptime', 'task_count', 'schedlag'] LOOP
        EXECUTE format($f$
            INSERT INTO %1$I ("time", "exit", "value")
            SELECT date_trunc(%4$L, "time"), coalesce("exit", ''), avg("value")
            FROM %2$I WHERE "time" >= %3$L GROUP BY 1, 2
            ON CONFLICT ("time", "exit") DO UPDATE SET "value" = EXCLUDED."value"
        $f$, gauge_table || suffix, gauge_table, cutoff, trunc_unit);
    END LOOP;

    -- Counter: sum per bucket.
    EXECUTE format($f$
        INSERT INTO %1$I ("time", "pool", "asn", "country", "value")
        SELECT date_trunc(%3$L, "time"), coalesce("pool", ''), coalesce("asn", ''), coalesce("country", ''), sum("value")
        FROM "bridge_bytes" WHERE "time" >= %2$L GROUP BY 1, 2, 3, 4
        ON CONFLICT ("time", "pool", "asn", "country") DO UPDATE SET "value" = EXCLUDED."value"
    $f$, 'bridge_bytes' || suffix, cutoff, trunc_unit);

    EXECUTE format($f$
        INSERT INTO %1$I ("time", "pool", "value")
        SELECT date_trunc(%3$L, "time"), coalesce("pool", ''), avg("value")
        FROM "bridge_pools" WHERE "time" >= %2$L GROUP BY 1, 2
        ON CONFLICT ("time", "pool") DO UPDATE SET "value" = EXCLUDED."value"
    $f$, 'bridge_pools' || suffix, cutoff, trunc_unit);

    EXECUTE format($f$
        INSERT INTO %1$I ("time", "kind", "value")
        SELECT date_trunc(%3$L, "time"), coalesce("kind", ''), avg("value")
        FROM "broker_logins" WHERE "time" >= %2$L GROUP BY 1, 2
        ON CONFLICT ("time", "kind") DO UPDATE SET "value" = EXCLUDED."value"
    $f$, 'broker_logins' || suffix, cutoff, trunc_unit);

    EXECUTE format($f$
        INSERT INTO %1$I ("time", "value")
        SELECT date_trunc(%3$L, "time"), avg("value")
        FROM "plus" WHERE "time" >= %2$L GROUP BY 1
        ON CONFLICT ("time") DO UPDATE SET "value" = EXCLUDED."value"
    $f$, 'plus' || suffix, cutoff, trunc_unit);

    EXECUTE format($f$
        INSERT INTO %1$I ("time", "ip_addr", "value")
        SELECT date_trunc(%3$L, "time"), coalesce("ip_addr", ''), avg("value")
        FROM "broker_sysstat" WHERE "time" >= %2$L GROUP BY 1, 2
        ON CONFLICT ("time", "ip_addr") DO UPDATE SET "value" = EXCLUDED."value"
    $f$, 'broker_sysstat' || suffix, cutoff, trunc_unit);

    -- Timer: count-weighted mean, worst-case upper/p99.
    EXECUTE format($f$
        INSERT INTO %1$I ("time", "method", "count", "mean", "upper", "p99")
        SELECT date_trunc(%3$L, "time"), coalesce("method", ''),
               sum("count"),
               sum("mean" * "count") / nullif(sum("count"), 0),
               max("upper"),
               max("99_percentile")
        FROM "broker_rpc_calls" WHERE "time" >= %2$L GROUP BY 1, 2
        ON CONFLICT ("time", "method") DO UPDATE
            SET "count" = EXCLUDED."count", "mean" = EXCLUDED."mean",
                "upper" = EXCLUDED."upper", "p99" = EXCLUDED."p99"
    $f$, 'broker_rpc_calls' || suffix, cutoff, trunc_unit);
END $$;

CREATE OR REPLACE FUNCTION metrics_rollup(since timestamptz DEFAULT now() - interval '3 hours')
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM metrics_rollup_tier('minute', '_minutely', date_trunc('minute', since));
    PERFORM metrics_rollup_tier('hour', '_hourly', date_trunc('hour', since));
END $$;

------------------------------------------------------------------------------
-- Retention: raw 14 days, minutely 90 days (history lives on hourly forever).
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION metrics_retention()
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'kbps', 'load', 'uptime', 'task_count', 'schedlag',
        'bridge_bytes', 'bridge_pools', 'broker_logins', 'plus',
        'broker_sysstat', 'broker_rpc_calls'
    ] LOOP
        EXECUTE format('DELETE FROM %I WHERE "time" < now() - interval ''14 days''', t);
        EXECUTE format('DELETE FROM %I WHERE "time" < now() - interval ''90 days''', t || '_minutely');
    END LOOP;
END $$;

------------------------------------------------------------------------------
-- metric(): Graphite-style resolution transparency for Grafana. Picks the raw
-- table for short, fine-grained ranges and the hourly rollup otherwise, then
-- buckets and aggregates. Tag columns come back as a jsonb object.
--
-- Example Grafana query:
--   SELECT "time", tags->>'exit' AS exit, value
--   FROM metric('kbps', $__timeFrom(), $__timeTo(), $__interval::interval)
-- For counters pass agg => 'sum'; for timer tables pass field => 'mean' etc.
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION metric(
    p_name text,
    p_from timestamptz,
    p_to timestamptz,
    p_bucket interval,
    p_agg text DEFAULT 'avg',
    p_field text DEFAULT 'value'
) RETURNS TABLE ("time" timestamptz, tags jsonb, value double precision)
LANGUAGE plpgsql STABLE AS $$
DECLARE
    raw_lo timestamptz;
    min_lo timestamptz;
    min_hi timestamptz;
    b1 timestamptz;  -- hourly/minutely boundary
    b2 timestamptz;  -- minutely/raw boundary
    tag_pairs text;
    tags_expr text;
    tag_cols text;
    inner_cols text;
    src_sql text;
BEGIN
    IF p_agg NOT IN ('avg', 'sum', 'max', 'min') THEN
        RAISE EXCEPTION 'metric(): unsupported aggregation %', p_agg;
    END IF;

    -- Tags = text columns present in BOTH raw and rollup tables (the rollups
    -- share a schema), so stray columns Telegraf may have auto-added to the
    -- raw table can't break the union below.
    SELECT string_agg(format('%L, %I', r.column_name, r.column_name), ', ' ORDER BY r.column_name),
           string_agg(quote_ident(r.column_name), ', ' ORDER BY r.column_name)
    INTO tag_pairs, tag_cols
    FROM information_schema.columns r
    JOIN information_schema.columns h
      ON h.table_schema = 'public' AND h.table_name = p_name || '_hourly'
     AND h.column_name = r.column_name AND h.data_type = 'text'
    WHERE r.table_schema = 'public' AND r.table_name = p_name AND r.data_type = 'text';

    tags_expr := coalesce('jsonb_build_object(' || tag_pairs || ')', $j$'{}'::jsonb$j$);
    inner_cols := '"time", ' || coalesce(tag_cols || ', ', '') || quote_ident(p_field);

    -- Whisper-style resolution mixing across three tiers. The result is the
    -- union of hourly [p_from, b1) + minutely [b1, b2) + raw [b2, p_to), with
    -- the boundaries placed by bucket size and actual tier coverage:
    --   bucket >= 1h          -> hourly everywhere (b1 = b2 = p_to)
    --   1min <= bucket < 1h   -> minutely where it exists, raw only for the
    --                            not-yet-rolled-up tail, hourly before minutely
    --   bucket < 1min         -> raw wherever raw exists, then minutely/hourly
    EXECUTE format('SELECT min("time") FROM %I', p_name) INTO raw_lo;
    EXECUTE format('SELECT min("time"), max("time") + interval ''1 minute'' FROM %I',
                   p_name || '_minutely') INTO min_lo, min_hi;
    raw_lo := coalesce(raw_lo, p_to);

    IF p_bucket >= interval '1 hour' THEN
        b1 := p_to;
        b2 := p_to;
    ELSIF p_bucket >= interval '1 minute' THEN
        b2 := coalesce(min_hi, raw_lo);
        b1 := least(coalesce(min_lo, b2), b2);
    ELSE
        b2 := raw_lo;
        b1 := least(coalesce(min_lo, b2), b2);
    END IF;

    src_sql := format(
        'SELECT %1$s FROM %2$I WHERE "time" >= %5$L AND "time" < %6$L
         UNION ALL
         SELECT %1$s FROM %3$I WHERE "time" >= %7$L AND "time" < %8$L
         UNION ALL
         SELECT %1$s FROM %4$I WHERE "time" >= %9$L AND "time" < %10$L',
        inner_cols, p_name || '_hourly', p_name || '_minutely', p_name,
        p_from, least(b1, p_to),
        greatest(p_from, b1), least(b2, p_to),
        greatest(p_from, b2), p_to);

    RETURN QUERY EXECUTE format(
        $q$ SELECT date_bin(%L, "time", timestamptz 'epoch') AS "time", %s AS tags,
                   %s(%I)::double precision AS value
            FROM (%s) __src
            GROUP BY 1, 2 ORDER BY 1 $q$,
        p_bucket, tags_expr, p_agg, p_field, src_sql);
END $$;

------------------------------------------------------------------------------
-- pg_cron schedules (cron.schedule upserts by job name).
------------------------------------------------------------------------------

SELECT cron.schedule('geph5-metrics-rollup', '*/10 * * * *', $$SELECT metrics_rollup()$$);
SELECT cron.schedule('geph5-metrics-retention', '20 4 * * *', $$SELECT metrics_retention()$$);
