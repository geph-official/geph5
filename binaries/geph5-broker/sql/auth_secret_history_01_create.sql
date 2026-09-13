-- Run after the hash cutover, under the broker startup rollout lock.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';
SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '2s';

CREATE TABLE IF NOT EXISTS public.auth_secret_history (
    secret_hash BYTEA PRIMARY KEY CHECK (octet_length(secret_hash) = 32),
    user_id INTEGER NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    retired_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS auth_secret_history_user_id
    ON public.auth_secret_history (user_id);

-- Match the account table's grants, including the broker's runtime role.
DO $$
DECLARE
    permission RECORD;
BEGIN
    FOR permission IN
        SELECT a.* FROM pg_class c
        CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a
        WHERE c.oid = 'public.auth_secret_hash'::regclass
    LOOP
        EXECUTE format('GRANT %s ON TABLE public.auth_secret_history TO %s%s',
            permission.privilege_type,
            CASE WHEN permission.grantee = 0 THEN 'PUBLIC'
                 ELSE quote_ident(pg_get_userbyid(permission.grantee)) END,
            CASE WHEN permission.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
END
$$;
COMMIT;
