-- From the broker directory, run with psql -X -v ON_ERROR_STOP=1 -f tests/rotation_rewards.sql on an empty test DB.
CREATE TABLE users (id INTEGER PRIMARY KEY);
CREATE TABLE auth_secret_hash (id INTEGER PRIMARY KEY);
CREATE TABLE auth_secret_history (user_id INTEGER, retired_at TIMESTAMPTZ DEFAULT NOW());
CREATE TABLE plus_periods (
    period_id SERIAL PRIMARY KEY, user_id INTEGER, start_time TIMESTAMPTZ,
    end_time TIMESTAMPTZ, tier INTEGER
);
\ir ../sql/auth_secret_rotation_rewards_01_create.sql
-- Check repeated schema rollout too.
\ir ../sql/auth_secret_rotation_rewards_01_create.sql
BEGIN;
INSERT INTO users SELECT generate_series(1, 7);
INSERT INTO auth_secret_history (user_id) SELECT generate_series(1, 6);
-- Duplicate history must still grant only once.
INSERT INTO auth_secret_history (user_id) VALUES (1);
INSERT INTO plus_periods (user_id, start_time, end_time, tier) VALUES
    (1, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 1),
    (2, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 0),
    (3, NOW() - INTERVAL '10 days', NOW() - INTERVAL '1 day', 1),
    (4, NOW() + INTERVAL '1 day', NOW() + INTERVAL '10 days', 1),
    (6, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 1),
    (6, NOW() + INTERVAL '10 days', NOW() + INTERVAL '20 days', 1),
    (7, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 1);
\set reward_sql `cat sql/auth_secret_rotation_rewards_02_grant.sql`
PREPARE reward(integer) AS :reward_sql
;
-- New rotation, rollback, retry, then historical backfill and restart.
SAVEPOINT rotation;
EXECUTE reward(1);
ROLLBACK TO rotation;
DO $$ BEGIN
    ASSERT NOT EXISTS (SELECT FROM auth_secret_rotation_rewards);
    ASSERT (SELECT count(*) FROM plus_periods) = 7;
END $$;
EXECUTE reward(1);
EXECUTE reward(1);
EXECUTE reward(NULL);
EXECUTE reward(NULL);
DO $$ BEGIN
    ASSERT (SELECT count(*) FROM auth_secret_rotation_rewards) = 6;
    ASSERT (SELECT count(*) FROM auth_secret_rotation_rewards WHERE granted) = 3;
    ASSERT (SELECT count(*) FROM plus_periods) = 10;
    ASSERT (SELECT max(end_time) FROM plus_periods WHERE user_id = 2 AND tier = 0) = NOW() + INTERVAL '17 days';
    ASSERT NOT EXISTS (SELECT FROM plus_periods WHERE user_id = 2 AND tier = 1);
    ASSERT (SELECT max(end_time) FROM plus_periods WHERE user_id = 1) = NOW() + INTERVAL '17 days';
    ASSERT (SELECT max(end_time) FROM plus_periods WHERE user_id = 6) = NOW() + INTERVAL '27 days';
    ASSERT (SELECT start_time FROM plus_periods WHERE user_id = 6 ORDER BY period_id DESC LIMIT 1) = NOW() + INTERVAL '20 days';
END $$;
-- Buying Plus after an ineligible rotation must not grant a bonus on restart.
INSERT INTO plus_periods (user_id, start_time, end_time, tier)
VALUES (5, NOW(), NOW() + INTERVAL '30 days', 1);
EXECUTE reward(NULL);
DO $$ BEGIN
    ASSERT (SELECT count(*) FROM plus_periods) = 11;
    ASSERT NOT (SELECT granted FROM auth_secret_rotation_rewards WHERE user_id = 5);
END $$;
-- Simulate accounts processed by the original Plus-only version.
INSERT INTO users SELECT generate_series(8, 12);
INSERT INTO auth_secret_history (user_id) SELECT generate_series(8, 12);
INSERT INTO auth_secret_rotation_rewards (user_id, processed_at, granted) VALUES
    (8, NOW() - INTERVAL '2 days', FALSE),
    (9, NOW() - INTERVAL '2 days', FALSE),
    (10, NOW() - INTERVAL '2 days', TRUE);
INSERT INTO plus_periods (user_id, start_time, end_time, tier) VALUES
    (8, NOW() - INTERVAL '3 days', NOW() + INTERVAL '10 days', 0),
    (9, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 0),
    (10, NOW() - INTERVAL '3 days', NOW() + INTERVAL '10 days', 1),
    (11, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 0),
    (11, NOW() + INTERVAL '10 days', NOW() + INTERVAL '20 days', 1),
    (12, NOW() - INTERVAL '1 day', NOW() + INTERVAL '20 days', 0),
    (12, NOW() - INTERVAL '1 day', NOW() + INTERVAL '10 days', 1);
EXECUTE reward(NULL);
EXECUTE reward(NULL);
DO $$ BEGIN
    ASSERT (SELECT count(*) FROM plus_periods) = 21;
    ASSERT (SELECT granted FROM auth_secret_rotation_rewards WHERE user_id = 8);
    ASSERT (SELECT max(end_time) FROM plus_periods WHERE user_id = 8 AND tier = 0) = NOW() + INTERVAL '17 days';
    ASSERT NOT (SELECT granted FROM auth_secret_rotation_rewards WHERE user_id = 9);
    ASSERT (SELECT count(*) FROM plus_periods WHERE user_id = 10) = 1;
    ASSERT (SELECT max(end_time) FROM plus_periods WHERE user_id = 11 AND tier = 0) = NOW() + INTERVAL '27 days';
    ASSERT (SELECT max(end_time) FROM plus_periods WHERE user_id = 12 AND tier = 1) = NOW() + INTERVAL '17 days';
    ASSERT (SELECT count(*) FROM plus_periods WHERE user_id = 12 AND tier = 0) = 1;
END $$;
ROLLBACK;
