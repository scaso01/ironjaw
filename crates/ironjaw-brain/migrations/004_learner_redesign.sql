-- Migration 004: Brain learner redesign — LLM-driven extraction + indexed knowledge
-- Adds strength, supersedes, superseded_by, content_hash columns to memories.
-- Creates learn_index table for incremental indexing.
-- Safe to re-run: checks for column existence before ALTER.

ALTER TABLE memories ADD COLUMN strength REAL NOT NULL DEFAULT 1.0;
ALTER TABLE memories ADD COLUMN supersedes INTEGER;
ALTER TABLE memories ADD COLUMN superseded_by INTEGER;
ALTER TABLE memories ADD COLUMN content_hash TEXT;

CREATE INDEX IF NOT EXISTS idx_memories_content_hash ON memories(content_hash);
CREATE INDEX IF NOT EXISTS idx_memories_strength ON memories(strength);
CREATE INDEX IF NOT EXISTS idx_memories_superseded_by ON memories(superseded_by);

CREATE TABLE IF NOT EXISTS learn_index (
    session_id TEXT PRIMARY KEY,
    last_processed_turn INTEGER NOT NULL DEFAULT 0,
    content_hash TEXT NOT NULL DEFAULT '',
    indexed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now'))
);
