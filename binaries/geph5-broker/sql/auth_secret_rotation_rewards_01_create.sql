-- Run after the hash cutover, under the broker startup rollout lock.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';
SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '2s';

CREATE TABLE IF NOT EXISTS public.auth_secret_rotation_rewards (
    user_id INTEGER PRIMARY KEY REFERENCES public.users(id) ON DELETE CASCADE,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    granted BOOLEAN NOT NULL
);

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
        EXECUTE format('GRANT %s ON TABLE public.auth_secret_rotation_rewards TO %s%s',
            permission.privilege_type,
            CASE WHEN permission.grantee = 0 THEN 'PUBLIC'
                 ELSE quote_ident(pg_get_userbyid(permission.grantee)) END,
            CASE WHEN permission.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
END
$$;
COMMIT;
