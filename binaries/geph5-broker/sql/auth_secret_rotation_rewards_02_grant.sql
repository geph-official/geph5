-- $1 is one rotating user, or NULL for the startup backfill.
-- Record ineligible rotations too: buying a subscription later must not earn a reward.
-- The claim and ledger entry share a statement and the caller's transaction.
WITH candidates AS (
    SELECT DISTINCT h.user_id
    FROM public.auth_secret_history h
    LEFT JOIN public.auth_secret_rotation_rewards r ON r.user_id = h.user_id
    WHERE ($1::integer IS NULL OR h.user_id = $1)
      AND (r.user_id IS NULL OR (
          -- Recover Basic users excluded by the original Plus-only rollout.
          -- Use their original evaluation time so later purchases do not qualify.
          NOT r.granted AND EXISTS (
              SELECT 1 FROM public.plus_periods p
              WHERE p.user_id = h.user_id AND p.tier = 0
                AND p.start_time <= r.processed_at AND p.end_time > r.processed_at
          )
      ))
), eligibility AS (
    SELECT c.user_id,
        (
            SELECT MAX(p.tier) FROM public.plus_periods p
            WHERE p.user_id = c.user_id AND p.tier IN (0, 1)
              AND p.start_time <= NOW() AND p.end_time > NOW()
        ) AS reward_tier
    FROM candidates c
), claimed AS (
    INSERT INTO public.auth_secret_rotation_rewards (user_id, granted)
    SELECT user_id, reward_tier IS NOT NULL FROM eligibility
    ON CONFLICT (user_id) DO UPDATE SET granted = TRUE
    WHERE NOT auth_secret_rotation_rewards.granted AND EXCLUDED.granted
    RETURNING user_id, granted
)
INSERT INTO public.plus_periods (user_id, start_time, end_time, tier)
SELECT c.user_id, MAX(p.end_time), MAX(p.end_time) + INTERVAL '168 hours', e.reward_tier
FROM claimed c
JOIN eligibility e ON e.user_id = c.user_id
-- Match billing: append Plus after Plus, Basic after all paid periods so
-- scheduled Plus does not mask the Basic reward.
JOIN public.plus_periods p ON p.user_id = c.user_id
    AND (e.reward_tier = 0 OR p.tier = e.reward_tier)
WHERE c.granted
GROUP BY c.user_id, e.reward_tier;
