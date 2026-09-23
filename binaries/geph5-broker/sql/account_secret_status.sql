-- Existing GUIs require Current to offer the normal upgrade screen.
-- Expose that path for the first 12 hours of the 24-hour recovery period.
-- Credential validation does not use this compatibility status.
SELECT s.id, false AS retired, i.code
FROM auth_secret_hash s
LEFT JOIN invite_codes i ON i.user_id = s.id
WHERE s.secret_hash = $1
UNION ALL
SELECT h.user_id, r.secret_hash IS NULL AS retired, i.code
FROM auth_secret_history h
LEFT JOIN auth_secret_recovery r
    ON r.secret_hash = h.secret_hash
    AND r.expires_at > clock_timestamp() + INTERVAL '12 hours'
LEFT JOIN invite_codes i ON i.user_id = h.user_id
WHERE h.secret_hash = $1
