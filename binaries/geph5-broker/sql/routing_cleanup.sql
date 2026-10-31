-- Installed automatically by brokers at startup under a transaction advisory lock.
-- Requires the existing pg_cron extension. Reapplying updates the named job.
-- pg_cron runs at most one instance of a given job at a time.
SELECT cron.schedule(
    'geph-expired-record-cleanup',
    '* * * * *',
    $job$
    SET LOCAL lock_timeout = '1s';
    SET LOCAL statement_timeout = '10s';
    DELETE FROM exits_new WHERE expiry < extract(epoch FROM now());
    DELETE FROM bridges_new WHERE expiry < extract(epoch FROM now());
    DELETE FROM auth_secret_recovery WHERE expires_at <= clock_timestamp();
    $job$
);

-- Ordinary autovacuum handles reclamation. Never VACUUM FULL live routing tables.
-- spent_bw_tokens must not be pruned: current tokens have no expiry and pruning
-- their redemption records would permit replay.
