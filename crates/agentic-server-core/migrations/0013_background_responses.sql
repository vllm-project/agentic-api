-- Background responses are stored as `queued` at admission and finalized by the job that owns them.
-- A foreground response keeps all three columns NULL. The owner renews its lease while the job runs;
-- a row whose lease has expired belongs to a stopped gateway instance and can be failed by any instance.
ALTER TABLE responses ADD COLUMN background_status TEXT;
ALTER TABLE responses ADD COLUMN background_owner TEXT;
ALTER TABLE responses ADD COLUMN background_lease_expires_at BIGINT;

-- Startup recovery scans only active background rows, ordered by lease expiry.
CREATE INDEX idx_responses_background_active ON responses (background_lease_expires_at)
    WHERE background_status IN ('queued', 'in_progress');
