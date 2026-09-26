-- AMUX-5237: named clearance signals.
--
-- A card waits on a named signal by carrying `blocked_on = 'signal:<name>'`
-- (no new issues column: every dispatch consumer already honours blocked_on).
-- This table is the other half: the durable record that a signal WAS raised,
-- by whom, when, with what note, and which cards the raise cleared. It is what
-- replaced regex-polling a peer's pane for "clearance given" (gs-3 false
-- positive) and prose approvals nothing parsed (mixpeek-frustrations, ~145
-- cards held on an approval already given).
--
-- raised_at is unix SECONDS (declared in invariants TIMESTAMP_COLUMNS).
-- cleared is a JSON array of {"card","session"} objects.
--
-- IF NOT EXISTS so `signals::ensure_signal_table` can replay it safely.
CREATE TABLE IF NOT EXISTS board_signals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    raised_by TEXT NOT NULL,
    raised_at INTEGER NOT NULL,
    note TEXT,
    cleared TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_board_signals_name ON board_signals(name, raised_at);
