-- Embedded and executed by the broker before serving requests.
-- Each file owns its transaction; never wrap both in an outer transaction.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';
SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '2s';

-- Preserve the existing schema's token-to-user mapping. In particular, the
-- source has no foreign key: adding one here could reject existing tokens.
CREATE TABLE public.auth_token_hash (
    token_hash BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    user_id INTEGER NOT NULL
);
CREATE INDEX auth_token_hash_user_id ON public.auth_token_hash (user_id);

DO $$
DECLARE
    permission RECORD;
BEGIN
    FOR permission IN
        SELECT a.*
        FROM pg_class c
        CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a
        WHERE c.oid = 'public.auth_tokens'::regclass
    LOOP
        EXECUTE format('GRANT %s ON TABLE public.auth_token_hash TO %s%s',
            permission.privilege_type,
            CASE WHEN permission.grantee = 0 THEN 'PUBLIC'
                 ELSE quote_ident(pg_get_userbyid(permission.grantee)) END,
            CASE WHEN permission.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
END
$$;

COMMIT;
