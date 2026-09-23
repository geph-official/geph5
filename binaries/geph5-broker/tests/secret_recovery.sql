-- Run with psql -X -v ON_ERROR_STOP=1 -f tests/secret_recovery.sql on an empty test DB.
CREATE TABLE users (id INTEGER PRIMARY KEY);
CREATE TABLE auth_secret_hash (id INTEGER PRIMARY KEY, secret_hash BYTEA);
CREATE TABLE invite_codes (user_id INTEGER PRIMARY KEY, code TEXT);
\ir ../sql/auth_secret_history_01_create.sql
\ir ../sql/auth_secret_recovery_01_create.sql
\ir ../sql/auth_secret_recovery_01_create.sql

INSERT INTO users VALUES (1), (2);
INSERT INTO auth_secret_history (secret_hash, user_id)
VALUES (decode(repeat('01', 32), 'hex'), 1),
       (decode(repeat('02', 32), 'hex'), 2);
BEGIN;
INSERT INTO auth_secret_recovery (secret_hash, replacement)
VALUES (decode(repeat('01', 32), 'hex'), decode(repeat('03', 52), 'hex'));
ROLLBACK;
DO $$ BEGIN
    ASSERT NOT EXISTS (SELECT FROM auth_secret_recovery);
END $$;
INSERT INTO auth_secret_recovery (secret_hash, replacement)
VALUES (decode(repeat('01', 32), 'hex'), decode(repeat('03', 52), 'hex'));
-- A fresh connection can recover the committed replacement.
\connect
DO $$ BEGIN
    ASSERT (SELECT replacement FROM auth_secret_recovery
            WHERE secret_hash=decode(repeat('01', 32), 'hex')
              AND expires_at > clock_timestamp()) = decode(repeat('03', 52), 'hex');
    ASSERT (SELECT expires_at BETWEEN clock_timestamp() + INTERVAL '23 hours 59 minutes'
                                 AND clock_timestamp() + INTERVAL '24 hours'
            FROM auth_secret_recovery);
END $$;
-- Exercise the exact query embedded in the status endpoint. Set deadlines from
-- statement time so the boundary cases have elapsed when the query checks them.
\set status_sql `cat sql/account_secret_status.sql`
CREATE FUNCTION test_account_status(bytea)
RETURNS TABLE (id INTEGER, retired BOOLEAN, code TEXT)
LANGUAGE SQL AS :'status_sql';
INSERT INTO users VALUES (3);
INSERT INTO auth_secret_hash VALUES (3, decode(repeat('05', 32), 'hex'));
INSERT INTO invite_codes VALUES (1, 'preserved-invite'), (3, 'current-invite');
DO $$
DECLARE
    old_hash BYTEA := decode(repeat('01', 32), 'hex');
    elapsed_hours INTEGER;
BEGIN
    ASSERT (SELECT NOT retired AND id=3 AND code='current-invite'
            FROM test_account_status(decode(repeat('05', 32), 'hex')));
    ASSERT NOT EXISTS (SELECT FROM test_account_status(decode(repeat('ff', 32), 'hex')));
    ASSERT (SELECT retired FROM test_account_status(decode(repeat('02', 32), 'hex')));
    FOREACH elapsed_hours IN ARRAY ARRAY[0, 11, 12, 13, 24] LOOP
        UPDATE auth_secret_recovery
        SET expires_at = statement_timestamp() + make_interval(hours => 24 - elapsed_hours)
        WHERE secret_hash = old_hash;
        ASSERT (SELECT retired = (elapsed_hours >= 12)
                FROM test_account_status(old_hash));
        IF elapsed_hours < 12 THEN
            ASSERT (SELECT id=1 AND code='preserved-invite' FROM test_account_status(old_hash));
        END IF;
        ASSERT (SELECT (expires_at > clock_timestamp()) = (elapsed_hours < 24)
                FROM auth_secret_recovery WHERE secret_hash = old_hash);
        -- Advertising Current must not restore the old authentication hash.
        ASSERT NOT EXISTS (SELECT FROM auth_secret_hash WHERE secret_hash = old_hash);
    END LOOP;
END $$;
-- An expired entry is inaccessible even before GC.
UPDATE auth_secret_recovery SET expires_at = clock_timestamp();
INSERT INTO auth_secret_recovery (secret_hash, replacement)
VALUES (decode(repeat('02', 32), 'hex'), decode(repeat('04', 52), 'hex'));
DO $$ BEGIN
    ASSERT NOT EXISTS (SELECT FROM auth_secret_recovery
                      WHERE secret_hash=decode(repeat('01', 32), 'hex')
                        AND expires_at > clock_timestamp());
END $$;
DELETE FROM auth_secret_recovery WHERE expires_at <= clock_timestamp();
DO $$ BEGIN
    ASSERT (SELECT count(*) FROM auth_secret_recovery) = 1;
    ASSERT (SELECT count(*) FROM auth_secret_history) = 2;
END $$;
DELETE FROM users WHERE id=2;
DO $$ BEGIN
    ASSERT NOT EXISTS (SELECT FROM auth_secret_recovery);
END $$;
