-- Indexes for window_stats queries that were full-scanning on every 3s tick.
-- Measured: window_stats took 194s (p50 1.8s, max 30s) against a 4.7 GB DB
-- because _amux_state_events and _amux_attempts had no index on their time
-- columns. These three indexes cover every WHERE clause in window_stats.

CREATE INDEX IF NOT EXISTS idx_amux_state_events_at
    ON _amux_state_events(at);

CREATE INDEX IF NOT EXISTS idx_amux_attempts_at
    ON _amux_attempts(at);

CREATE INDEX IF NOT EXISTS idx_amux_verifications_created_at
    ON _amux_verifications(created_at);
