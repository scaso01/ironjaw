PRAGMA journal_mode=WAL;

CREATE TABLE IF NOT EXISTS schedules (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL DEFAULT 'default',
    name        TEXT NOT NULL,
    cron_expr   TEXT NOT NULL,
    action_type TEXT NOT NULL,
    action_data TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_run    TEXT,
    next_run    TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE TABLE IF NOT EXISTS workflows (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL DEFAULT 'default',
    name        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    steps_json  TEXT NOT NULL,
    state_json  TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE TABLE IF NOT EXISTS workflow_steps (
    id          TEXT PRIMARY KEY,
    workflow_id TEXT NOT NULL REFERENCES workflows(id),
    name        TEXT NOT NULL,
    step_type   TEXT NOT NULL,
    params_json TEXT NOT NULL,
    depends_on  TEXT,
    status      TEXT NOT NULL DEFAULT 'pending',
    result_json TEXT,
    started_at  TEXT,
    completed_at TEXT
);
