-- Embedded and executed by the broker before serving requests.
-- Each file owns its transaction; never wrap both in an outer transaction.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';

SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '2s';

-- Other account writers must be stopped for this coordinated cutover. Keep this
-- transaction separate from the backfill so its schema locks release first.
CREATE TABLE public.auth_secret_hash (
    id INTEGER PRIMARY KEY REFERENCES public.users(id) ON DELETE CASCADE,
    secret_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(secret_hash) = 32)
);

-- Preserve the original table's grants (including the support role's SELECT
-- access). The support functions use the caller's privileges.
DO $$
DECLARE
    permission RECORD;
BEGIN
    FOR permission IN
        SELECT a.*
        FROM pg_class c
        CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a
        WHERE c.oid = 'public.auth_secret'::regclass
    LOOP
        EXECUTE format('GRANT %s ON TABLE public.auth_secret_hash TO %s%s',
            permission.privilege_type,
            CASE WHEN permission.grantee = 0 THEN 'PUBLIC'
                 ELSE quote_ident(pg_get_userbyid(permission.grantee)) END,
            CASE WHEN permission.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
END
$$;

COMMIT;
