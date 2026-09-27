-- Standing approvals (AMUX-5270): answers the owner already gave, recorded
-- where every lane and the alert path can read them, instead of living only in
-- the terminal of the lane that asked (gs-4 decision #7, 2026-09-26).
CREATE TABLE IF NOT EXISTS standing_approvals (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    title          TEXT    NOT NULL,
    allowed        TEXT    NOT NULL,             -- the plain sentence of what is allowed
    category       TEXT    NOT NULL,             -- budget|customer_outbound|prod_data|credential|access|decision
    limits         TEXT    NOT NULL DEFAULT '',  -- free-text limits, shown verbatim
    max_per_day    INTEGER,                      -- structured cap: applied uses per rolling 24h
    max_amount_usd REAL,                         -- structured cap: largest $ figure an ask may name
    require_terms  TEXT    NOT NULL DEFAULT '',  -- comma list; the ask must contain one (precondition)
    scope          TEXT    NOT NULL DEFAULT 'global', -- global | group:<g> | worker:<w>
    granted_by     TEXT    NOT NULL DEFAULT 'owner',
    granted_at     INTEGER NOT NULL,
    source         TEXT    NOT NULL DEFAULT '',
    expires_at     INTEGER,
    revoked        INTEGER NOT NULL DEFAULT 0,
    revoked_at     INTEGER,
    revoked_by     TEXT
);

-- Every time an approval answered an ask (or would have, but its cap was
-- spent). This is the per-day counter AND the owner's FYI feed.
CREATE TABLE IF NOT EXISTS standing_approval_uses (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    approval_id INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    session     TEXT    NOT NULL DEFAULT '',
    door        TEXT    NOT NULL,               -- alert | needsyou
    verdict     TEXT    NOT NULL,               -- applied | cap_reached
    ask         TEXT    NOT NULL DEFAULT '',
    reference   TEXT    NOT NULL DEFAULT ''     -- card id for the needsyou door
);
CREATE INDEX IF NOT EXISTS idx_standing_approval_uses_approval_ts
    ON standing_approval_uses (approval_id, ts);
