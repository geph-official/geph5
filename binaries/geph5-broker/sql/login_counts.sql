-- Installed automatically by brokers at startup under a transaction advisory lock.
-- The initial population runs once; subsequent refreshes keep reads available.
CREATE MATERIALIZED VIEW IF NOT EXISTS public.broker_login_counts AS
SELECT
    true AS singleton,
    (SELECT count(id) FROM public.last_login
     WHERE login_time > now() - interval '24 hours') AS daily,
    (SELECT count(id) FROM public.last_login NATURAL JOIN public.auth_secret_hash
     WHERE login_time > now() - interval '24 hours') AS daily_new,
    (SELECT count(id) FROM public.last_login
     WHERE login_time > now() - interval '7 days') AS weekly,
    now() AS refreshed_at;

CREATE UNIQUE INDEX IF NOT EXISTS broker_login_counts_singleton
    ON public.broker_login_counts (singleton);

SELECT cron.schedule(
    'geph-login-counts',
    '*/5 * * * *',
    'REFRESH MATERIALIZED VIEW CONCURRENTLY public.broker_login_counts'
);
