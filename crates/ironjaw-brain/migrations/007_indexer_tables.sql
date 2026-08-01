-- Migration 007: Indexer sync tables for chronicle and markdown spokes.
-- sync_state: tracks incremental sync position per source.
-- markdown_index: tracks indexed markdown files and their content hashes.

CREATE TABLE IF NOT EXISTS sync_state (
    source TEXT PRIMARY KEY,
    last_synced_id TEXT NOT NULL DEFAULT '',
    last_synced_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);

CREATE TABLE IF NOT EXISTS markdown_index (
    file_path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    last_indexed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);
