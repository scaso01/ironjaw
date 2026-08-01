-- Ironjaw audit trail: append-only event log
CREATE TABLE IF NOT EXISTS audit_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     TEXT NOT NULL DEFAULT 'default',
    timestamp   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    action_type TEXT    NOT NULL,
    method      TEXT,
    tool_name   TEXT,
    params      TEXT,           -- JSON
    result      TEXT,           -- JSON (truncated for large results)
    ok          INTEGER NOT NULL DEFAULT 1,
    error       TEXT,
    llm_model   TEXT,
    tokens_in   INTEGER,
    tokens_out  INTEGER,
    cost_usd    REAL,
    latency_ms  INTEGER,
    session_id  TEXT,
    device_id   TEXT,
    request_id  TEXT NOT NULL    -- UUID linking to the protocol frame id
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_audit_timestamp   ON audit_log(timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_action_type ON audit_log(action_type);
CREATE INDEX IF NOT EXISTS idx_audit_session_id  ON audit_log(session_id);
CREATE INDEX IF NOT EXISTS idx_audit_request_id  ON audit_log(request_id);
CREATE INDEX IF NOT EXISTS idx_audit_user_id     ON audit_log(user_id);
