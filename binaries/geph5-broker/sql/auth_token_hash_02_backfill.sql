-- Embedded and executed by the broker before serving requests.
-- Old brokers and other token writers must be stopped during this cutover.
BEGIN;
SET LOCAL idle_in_transaction_session_timeout = '10s';
SET LOCAL lock_timeout = '250ms';
SET LOCAL statement_timeout = '5min';

INSERT INTO public.auth_token_hash (token_hash, user_id)
SELECT sha256(convert_to(token, 'UTF8')), user_id
FROM public.auth_tokens
ON CONFLICT (token_hash) DO UPDATE SET user_id = EXCLUDED.user_id;

SET LOCAL statement_timeout = '2s';
DROP TABLE public.auth_tokens RESTRICT;
COMMIT;
