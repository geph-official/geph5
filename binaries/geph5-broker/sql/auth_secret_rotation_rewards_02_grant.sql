-- $1 is one rotating user, or NULL for the startup backfill.
-- Record ineligible rotations too: buying Plus later must not earn a reward.
-- The claim and ledger entry share a statement and the caller's transaction.
WITH candidates AS (
    SELECT DISTINCT h.user_id
    FROM public.auth_secret_history h
    WHERE ($1::integer IS NULL OR h.user_id = $1)
      AND NOT EXISTS (
          SELECT 1 FROM public.auth_secret_rotation_rewards r WHERE r.user_id = h.user_id
      )
), eligibility AS (
    SELECT c.user_id,
        EXISTS (
            SELECT 1 FROM public.plus_periods p
            WHERE p.user_id = c.user_id AND p.tier = 1
              AND p.start_time <= NOW() AND p.end_time > NOW()
        ) AS granted
    FROM candidates c
), claimed AS (
    INSERT INTO public.auth_secret_rotation_rewards (user_id, granted)
    SELECT user_id, granted FROM eligibility
    ON CONFLICT (user_id) DO NOTHING
    RETURNING user_id, granted
)
INSERT INTO public.plus_periods (user_id, start_time, end_time, tier)
SELECT c.user_id, MAX(p.end_time), MAX(p.end_time) + INTERVAL '168 hours', 1
FROM claimed c
JOIN public.plus_periods p ON p.user_id = c.user_id AND p.tier = 1
WHERE c.granted
GROUP BY c.user_id;
