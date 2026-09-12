-- Embedded and executed by the broker before serving requests.
-- Each file owns its transaction; never wrap both in an outer transaction.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';

SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '5min';

INSERT INTO public.auth_secret_hash (id, secret_hash)
SELECT id, sha256(convert_to(secret, 'UTF8'))
FROM public.auth_secret
ON CONFLICT (id) DO NOTHING;

-- Refuse to discard plaintext if a pre-existing destination row disagrees.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM public.auth_secret s
        LEFT JOIN public.auth_secret_hash h ON h.id = s.id
        WHERE h.id IS NULL OR h.secret_hash <> sha256(convert_to(s.secret, 'UTF8'))
    ) THEN
        RAISE EXCEPTION 'account secret hash backfill verification failed';
    END IF;
END
$$;

-- Preserve the GUI's first 16 Crockford Base32 characters of
-- SHA256("invite-code" || secret). Only derive missing codes; existing codes
-- must keep their owners. The account hash itself has no prefix.
INSERT INTO public.invite_codes (user_id, code)
SELECT s.id, (
    SELECT string_agg(
        substr('0123456789ABCDEFGHJKMNPQRSTVWXYZ',
            substring(
                ('x' || encode(sha256(convert_to('invite-code' || s.secret, 'UTF8')), 'hex'))::bit(256)
                FROM (i * 5 + 1) FOR 5
            )::integer + 1, 1),
        '' ORDER BY i)
    FROM generate_series(0, 15) AS i
)
FROM public.auth_secret s
WHERE NOT EXISTS (SELECT 1 FROM public.invite_codes ic WHERE ic.user_id = s.id)
ON CONFLICT (user_id) DO NOTHING;

SET LOCAL statement_timeout = '2s';

CREATE OR REPLACE FUNCTION public.addplusbysecret(
    p_secret text, p_duration interval, p_tier integer DEFAULT 1
)
RETURNS void
LANGUAGE sql
AS $function$
    INSERT INTO public.plus_periods (user_id, start_time, end_time, tier)
    SELECT s.id,
        COALESCE(MAX(p.end_time) FILTER (WHERE p.end_time > NOW() AND p.tier = p_tier), NOW()),
        COALESCE(MAX(p.end_time) FILTER (WHERE p.end_time > NOW() AND p.tier = p_tier), NOW()) + p_duration,
        p_tier
    FROM public.auth_secret_hash s
    LEFT JOIN public.plus_periods p ON p.user_id = s.id
    WHERE s.secret_hash = sha256(convert_to(p_secret, 'UTF8'))
    GROUP BY s.id;
$function$;

CREATE OR REPLACE FUNCTION public.showplusbysecret(p_secret text)
RETURNS SETOF public.plus_periods
LANGUAGE sql
AS $function$
    SELECT pp.* FROM public.plus_periods pp
    WHERE pp.user_id = (
        SELECT s.id FROM public.auth_secret_hash s
        WHERE s.secret_hash = sha256(convert_to(p_secret, 'UTF8'))
    );
$function$;

CREATE OR REPLACE FUNCTION public.showrevenuebysecret(p_secret text)
RETURNS TABLE(eurocents bigint, method text, metadata jsonb, create_time timestamp with time zone)
LANGUAGE sql
AS $function$
    SELECT r.eurocents, r.method, r.metadata, pp.create_time
    FROM public.revenue r NATURAL JOIN public.plus_periods pp
    WHERE pp.user_id = (
        SELECT s.id FROM public.auth_secret_hash s
        WHERE s.secret_hash = sha256(convert_to(p_secret, 'UTF8'))
    );
$function$;

-- RESTRICT deliberately fails on unexpected dependencies. This and the
-- function replacements commit atomically with the verified backfill.
DROP TABLE public.auth_secret RESTRICT;

COMMIT;
