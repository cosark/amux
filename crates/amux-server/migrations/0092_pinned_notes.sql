-- Pinned notes: files the user can float as always-on-top macOS overlays.
CREATE TABLE IF NOT EXISTS pinned_notes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path   TEXT    NOT NULL,
    title       TEXT    NOT NULL DEFAULT '',
    pos_x       INTEGER NOT NULL DEFAULT 100,
    pos_y       INTEGER NOT NULL DEFAULT 100,
    width       INTEGER NOT NULL DEFAULT 400,
    height      INTEGER NOT NULL DEFAULT 500,
    opacity     REAL    NOT NULL DEFAULT 0.6,
    active      INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
