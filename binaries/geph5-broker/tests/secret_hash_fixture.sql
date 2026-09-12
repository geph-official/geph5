CREATE TABLE public.users (id SERIAL PRIMARY KEY, createtime TIMESTAMP NOT NULL DEFAULT NOW());
CREATE TABLE public.auth_secret (
    id INTEGER PRIMARY KEY REFERENCES public.users(id) ON DELETE CASCADE,
    secret TEXT NOT NULL UNIQUE
);
CREATE TABLE public.invite_codes (
    user_id INTEGER PRIMARY KEY REFERENCES public.users(id), code TEXT NOT NULL UNIQUE
);
CREATE TABLE public.plus_periods (
    period_id SERIAL PRIMARY KEY, user_id INTEGER REFERENCES public.users(id),
    start_time TIMESTAMPTZ NOT NULL, end_time TIMESTAMPTZ NOT NULL,
    tier INTEGER NOT NULL, create_time TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE public.revenue (
    period_id INTEGER REFERENCES public.plus_periods(period_id),
    eurocents BIGINT NOT NULL, method TEXT NOT NULL, metadata JSONB NOT NULL
);
CREATE TABLE public.auth_password (user_id INTEGER PRIMARY KEY, username TEXT, pwdhash TEXT);
CREATE TABLE public.auth_tokens (token TEXT PRIMARY KEY, user_id INTEGER);
-- The broker's manual scripts must not interact with payment migration history.
CREATE TABLE public._sqlx_migrations (version BIGINT PRIMARY KEY, checksum BYTEA NOT NULL);
INSERT INTO public._sqlx_migrations VALUES (123, decode('abcd', 'hex'));
INSERT INTO public.users DEFAULT VALUES;
INSERT INTO public.users DEFAULT VALUES;
INSERT INTO public.users DEFAULT VALUES;
INSERT INTO public.auth_secret VALUES
    (1, '900000000000000000000001'), (2, '000123'), (3, 'UTF8-密钥');
INSERT INTO public.invite_codes VALUES (2, 'EXISTING-CODE');
INSERT INTO public.auth_password VALUES (1, 'legacy', 'unchanged-password-hash');
INSERT INTO public.auth_tokens VALUES ('unchanged-token', 1);

CREATE FUNCTION public.addplusbysecret(p_secret TEXT, p_duration INTERVAL, p_tier INTEGER DEFAULT 1)
RETURNS VOID LANGUAGE SQL AS $$
    INSERT INTO public.plus_periods (user_id, start_time, end_time, tier)
    SELECT s.id,
        COALESCE(MAX(p.end_time) FILTER (WHERE p.end_time > NOW() AND p.tier = p_tier), NOW()),
        COALESCE(MAX(p.end_time) FILTER (WHERE p.end_time > NOW() AND p.tier = p_tier), NOW()) + p_duration,
        p_tier
    FROM public.auth_secret s LEFT JOIN public.plus_periods p ON p.user_id = s.id
    WHERE s.secret = p_secret GROUP BY s.id;
$$;
CREATE FUNCTION public.showplusbysecret(p_secret TEXT)
RETURNS SETOF public.plus_periods LANGUAGE SQL AS $$
    SELECT pp.* FROM public.plus_periods pp
    WHERE pp.user_id = (SELECT s.id FROM public.auth_secret s WHERE s.secret = p_secret);
$$;
CREATE FUNCTION public.showrevenuebysecret(p_secret TEXT)
RETURNS TABLE(eurocents BIGINT, method TEXT, metadata JSONB, create_time TIMESTAMPTZ)
LANGUAGE SQL AS $$
    SELECT r.eurocents, r.method, r.metadata, pp.create_time
    FROM public.revenue r NATURAL JOIN public.plus_periods pp
    WHERE pp.user_id = (SELECT s.id FROM public.auth_secret s WHERE s.secret = p_secret);
$$;
