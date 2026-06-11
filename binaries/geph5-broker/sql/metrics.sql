-- Geph5 metrics: a fully convention-driven pipeline. There is NO central list
-- of metric names anywhere in this file.
--
--   * Telegraf (statsd -> outputs.postgresql, schema = "metrics") creates one
--     table per measurement inside the `metrics` schema. Membership in that
--     schema is what makes something a metric.
--   * Every pg_cron rollup tick discovers raw tables by introspection,
--     auto-creates missing `<name>_minutely` / `<name>_hourly` tiers (and a
--     time index on the raw table), and upserts the trailing window.
--   * Aggregation semantics come from the data itself: the statsd
--     `metric_type` column (counter -> sum, gauge -> avg) plus structural
--     per-column rules for timer tables (count -> sum, mean -> count-weighted
--     mean, *percentile*/upper -> max, lower -> min).
--   * Retention is by suffix convention: raw 14 days, minutely 90 days,
--     hourly forever.
--
-- So emitting a brand-new stat from any geph5 binary materializes the table,
-- indexes, rollup tiers, retention, and metric() support within one rollup
-- tick, with zero configuration here or anywhere else.
--
-- Apply with: psql "$POSTGRES_URL" -f metrics.sql   (idempotent)

CREATE SCHEMA IF NOT EXISTS metrics;

------------------------------------------------------------------------------
-- One-time migration: move the original public-schema metric tables into the
-- metrics schema. Guarded; a no-op once they have moved. This is the only
-- place table names appear, and it exists purely to migrate legacy state.
------------------------------------------------------------------------------

DO $$
DECLARE
    t text;
    v text;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'kbps', 'load', 'uptime', 'task_count', 'schedlag', 'bridge_bytes',
        'bridge_pools', 'broker_logins', 'plus', 'broker_sysstat', 'broker_rpc_calls'
    ] LOOP
        FOREACH v IN ARRAY ARRAY[t, t || '_minutely', t || '_hourly'] LOOP
            IF to_regclass('public.' || quote_ident(v)) IS NOT NULL THEN
                EXECUTE format('ALTER TABLE public.%I SET SCHEMA metrics', v);
            END IF;
        END LOOP;
    END LOOP;
END $$;

------------------------------------------------------------------------------
-- Purge stray function variants left by earlier applies that ran through the
-- transaction pooler with an inconsistent search_path.
------------------------------------------------------------------------------

DO $purge$
DECLARE
    r record;
BEGIN
    FOR r IN
        SELECT n.nspname, p.proname, pg_get_function_identity_arguments(p.oid) AS args
        FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname IN ('public', 'metrics')
          AND (p.proname LIKE 'metrics\_%' OR (n.nspname = 'metrics' AND p.proname = 'metric'))
    LOOP
        EXECUTE format('DROP FUNCTION %I.%I(%s)', r.nspname, r.proname, r.args);
    END LOOP;
END $purge$;

------------------------------------------------------------------------------
-- Introspection helpers.
------------------------------------------------------------------------------

-- Raw metric tables: every base table in the metrics schema that has a "time"
-- column and is not itself a rollup tier.
CREATE OR REPLACE FUNCTION metrics.raw_tables()
RETURNS SETOF text LANGUAGE sql STABLE AS $$
    SELECT t.table_name::text
    FROM information_schema.tables t
    WHERE t.table_schema = 'metrics' AND t.table_type = 'BASE TABLE'
      AND t.table_name NOT LIKE '%\_minutely' ESCAPE '\'
      AND t.table_name NOT LIKE '%\_hourly' ESCAPE '\'
      AND EXISTS (SELECT 1 FROM information_schema.columns c
                  WHERE c.table_schema = 'metrics' AND c.table_name = t.table_name
                    AND c.column_name = 'time')
$$;

CREATE OR REPLACE FUNCTION metrics.text_cols(tbl text)
RETURNS text[] LANGUAGE sql STABLE AS $$
    SELECT coalesce(array_agg(column_name::text ORDER BY column_name), '{}')
    FROM information_schema.columns
    WHERE table_schema = 'metrics' AND table_name = tbl AND data_type = 'text'
$$;

CREATE OR REPLACE FUNCTION metrics.num_cols(tbl text)
RETURNS text[] LANGUAGE sql STABLE AS $$
    SELECT coalesce(array_agg(column_name::text ORDER BY column_name), '{}')
    FROM information_schema.columns
    WHERE table_schema = 'metrics' AND table_name = tbl
      AND column_name <> 'time'
      AND data_type IN ('double precision', 'real', 'bigint', 'integer', 'smallint', 'numeric')
$$;

-- The statsd type of a metric ('counter' | 'gauge' | 'timing'), read from the
-- data; NULL when unknown (e.g. rows written before metric_type was stored).
CREATE OR REPLACE FUNCTION metrics.type_of(tbl text)
RETURNS text LANGUAGE plpgsql STABLE AS $$
DECLARE
    result text;
BEGIN
    IF NOT ('metric_type' = ANY (metrics.text_cols(tbl))) THEN
        RETURN NULL;
    END IF;
    EXECUTE format(
        'SELECT metric_type FROM metrics.%I WHERE metric_type IS NOT NULL
         ORDER BY "time" DESC LIMIT 1', tbl) INTO result;
    RETURN result;
END $$;

-- How to aggregate one numeric column when downsampling. Structural rules
-- only; no per-metric configuration.
CREATE OR REPLACE FUNCTION metrics.agg_expr(col text, mtype text, cols text[])
RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN col = 'count' THEN format('sum(%I)', col)
        WHEN col = 'sum' THEN format('sum(%I)', col)
        WHEN col = 'mean' AND 'count' = ANY (cols)
            THEN format('sum(%I * "count") / nullif(sum("count"), 0)', col)
        WHEN col LIKE '%percentile%' OR col IN ('upper', 'max') THEN format('max(%I)', col)
        WHEN col IN ('lower', 'min') THEN format('min(%I)', col)
        WHEN mtype = 'counter' THEN format('sum(%I)', col)
        ELSE format('avg(%I)', col)
    END
$$;

------------------------------------------------------------------------------
-- Tier management: create missing rollup tables (and the raw time index)
-- from the raw table's own shape. Tag columns are NOT NULL DEFAULT '' so they
-- can participate in the primary key that ON CONFLICT upserts against.
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION metrics.ensure_tiers(raw text)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    suffix text;
    col text;
    cols_sql text;
BEGIN
    EXECUTE format('CREATE INDEX IF NOT EXISTS %I ON metrics.%I ("time")',
                   raw || '_time_idx', raw);
    FOREACH suffix IN ARRAY ARRAY['_minutely', '_hourly'] LOOP
        CONTINUE WHEN to_regclass('metrics.' || quote_ident(raw || suffix)) IS NOT NULL;
        cols_sql := '"time" timestamptz NOT NULL';
        FOREACH col IN ARRAY metrics.text_cols(raw) LOOP
            cols_sql := cols_sql || format(', %I text NOT NULL DEFAULT %L', col, '');
        END LOOP;
        FOREACH col IN ARRAY metrics.num_cols(raw) LOOP
            cols_sql := cols_sql || format(', %I double precision', col);
        END LOOP;
        cols_sql := cols_sql || format(', PRIMARY KEY (%s)',
            (SELECT string_agg(quote_ident(c), ', ')
             FROM unnest('{time}'::text[] || metrics.text_cols(raw)) AS c));
        EXECUTE format('CREATE TABLE metrics.%I (%s)', raw || suffix, cols_sql);
    END LOOP;
END $$;

------------------------------------------------------------------------------
-- Rollup: upsert the trailing window into one tier. Only columns present in
-- BOTH the raw table and the tier participate, so hand-crafted or historical
-- tier tables with fewer columns keep working.
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION metrics.rollup_tier(raw text, trunc_unit text, suffix text, cutoff timestamptz)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    tier text := raw || suffix;
    mtype text := metrics.type_of(raw);
    raw_nums text[] := metrics.num_cols(raw);
    tags text[];
    nums text[];
    col text;
    sel_tags text := '';
    ins_cols text := '"time"';
    sel_aggs text := '';
    upd_sets text := '';
    conflict_cols text;
BEGIN
    -- intersect with the tier's columns
    SELECT coalesce(array_agg(c ORDER BY c), '{}') INTO tags
    FROM unnest(metrics.text_cols(raw)) c WHERE c = ANY (metrics.text_cols(tier));
    SELECT coalesce(array_agg(c ORDER BY c), '{}') INTO nums
    FROM unnest(raw_nums) c WHERE c = ANY (metrics.num_cols(tier));
    IF array_length(nums, 1) IS NULL THEN
        RETURN;
    END IF;

    FOREACH col IN ARRAY tags LOOP
        ins_cols := ins_cols || format(', %I', col);
        sel_tags := sel_tags || format(', coalesce(%I, %L)', col, '');
    END LOOP;
    FOREACH col IN ARRAY nums LOOP
        ins_cols := ins_cols || format(', %I', col);
        sel_aggs := sel_aggs || format(', %s', metrics.agg_expr(col, mtype, raw_nums));
        upd_sets := upd_sets || format('%s%I = EXCLUDED.%I',
                                       CASE WHEN upd_sets = '' THEN '' ELSE ', ' END, col, col);
    END LOOP;
    SELECT string_agg(quote_ident(c), ', ')
    INTO conflict_cols FROM unnest('{time}'::text[] || tags) AS c;

    EXECUTE format(
        $q$ INSERT INTO metrics.%I (%s)
            SELECT date_trunc(%L, "time")%s%s
            FROM metrics.%I WHERE "time" >= %L
            GROUP BY %s
            ON CONFLICT (%s) DO UPDATE SET %s $q$,
        tier, ins_cols, trunc_unit, sel_tags, sel_aggs, raw, cutoff,
        (SELECT string_agg((i)::text, ', ') FROM generate_series(1, 1 + coalesce(array_length(tags, 1), 0)) i),
        conflict_cols, upd_sets);
END $$;

CREATE OR REPLACE FUNCTION metrics.rollup(since timestamptz DEFAULT now() - interval '3 hours')
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    raw text;
BEGIN
    FOR raw IN SELECT metrics.raw_tables() LOOP
        PERFORM metrics.ensure_tiers(raw);
        PERFORM metrics.rollup_tier(raw, 'minute', '_minutely', date_trunc('minute', since));
        PERFORM metrics.rollup_tier(raw, 'hour', '_hourly', date_trunc('hour', since));
    END LOOP;
END $$;

------------------------------------------------------------------------------
-- Retention by suffix convention: raw 14 days, minutely 90 days.
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION metrics.retention()
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    raw text;
BEGIN
    FOR raw IN SELECT metrics.raw_tables() LOOP
        EXECUTE format('DELETE FROM metrics.%I WHERE "time" < now() - interval ''14 days''', raw);
        IF to_regclass('metrics.' || quote_ident(raw || '_minutely')) IS NOT NULL THEN
            EXECUTE format('DELETE FROM metrics.%I WHERE "time" < now() - interval ''90 days''',
                           raw || '_minutely');
        END IF;
    END LOOP;
END $$;

------------------------------------------------------------------------------
-- metric(): Graphite-style resolution transparency for Grafana. Lives in the
-- public schema (so panels call it unqualified) and reads the metrics schema.
--
--   SELECT "time", tags->>'exit' AS exit, value
--   FROM metric('kbps', $__timeFrom(), $__timeTo(), interval '$__interval')
--
-- For counters pass agg => 'sum'; for timer tables pass field => 'mean' etc.
------------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION public.metric(
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
    tag_cols text[];
    tags_expr text;
    inner_cols text;
    src_sql text;
BEGIN
    IF p_agg NOT IN ('avg', 'sum', 'max', 'min') THEN
        RAISE EXCEPTION 'metric(): unsupported aggregation %', p_agg;
    END IF;

    -- Tags = text columns present in both raw and rollup tiers, minus the
    -- statsd bookkeeping column.
    SELECT coalesce(array_agg(c ORDER BY c), '{}') INTO tag_cols
    FROM unnest(metrics.text_cols(p_name)) c
    WHERE c = ANY (metrics.text_cols(p_name || '_hourly')) AND c <> 'metric_type';

    SELECT coalesce('jsonb_build_object(' ||
               string_agg(format('%L, %I', c, c), ', ') || ')', $j$'{}'::jsonb$j$)
    INTO tags_expr FROM unnest(tag_cols) c;
    SELECT '"time", ' || coalesce(string_agg(quote_ident(c), ', ') || ', ', '') || quote_ident(p_field)
    INTO inner_cols FROM unnest(tag_cols) c;

    -- Whisper-style resolution mixing: hourly [p_from, b1) + minutely
    -- [b1, b2) + raw [b2, p_to), boundaries placed by bucket size and tier
    -- coverage. Buckets >= 1h read hourly only; >= 1min prefer minutely with
    -- raw filling the not-yet-rolled-up tail; finer buckets prefer raw.
    EXECUTE format('SELECT min("time") FROM metrics.%I', p_name) INTO raw_lo;
    EXECUTE format('SELECT min("time"), max("time") + interval ''1 minute'' FROM metrics.%I',
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
        'SELECT %1$s FROM metrics.%2$I WHERE "time" >= %5$L AND "time" < %6$L
         UNION ALL
         SELECT %1$s FROM metrics.%3$I WHERE "time" >= %7$L AND "time" < %8$L
         UNION ALL
         SELECT %1$s FROM metrics.%4$I WHERE "time" >= %9$L AND "time" < %10$L',
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

SELECT cron.schedule('geph5-metrics-rollup', '*/10 * * * *', $$SELECT metrics.rollup()$$);
SELECT cron.schedule('geph5-metrics-retention', '20 4 * * *', $$SELECT metrics.retention()$$);
