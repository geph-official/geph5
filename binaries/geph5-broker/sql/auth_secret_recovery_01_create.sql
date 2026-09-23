-- Run after the hash cutover, under the broker startup rollout lock.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';
SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '2s';

CREATE TABLE IF NOT EXISTS public.auth_secret_recovery (
    secret_hash BYTEA PRIMARY KEY REFERENCES public.auth_secret_history(secret_hash) ON DELETE CASCADE,
    -- Random nonce followed by ciphertext and authentication tag.
    replacement BYTEA NOT NULL CHECK (octet_length(replacement) = 52),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (clock_timestamp() + INTERVAL '24 hours')
);
CREATE INDEX IF NOT EXISTS auth_secret_recovery_expiry
    ON public.auth_secret_recovery (expires_at);

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
        EXECUTE format('GRANT %s ON TABLE public.auth_secret_recovery TO %s%s',
            permission.privilege_type,
            CASE WHEN permission.grantee = 0 THEN 'PUBLIC'
                 ELSE quote_ident(pg_get_userbyid(permission.grantee)) END,
            CASE WHEN permission.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
END
$$;
COMMIT;
