-- Migration 003: Typed memories with importance scoring
-- Adds memory_type, importance, and related_to columns to the memories table.
-- Safe to re-run: checks for column existence before ALTER.

ALTER TABLE memories ADD COLUMN memory_type TEXT NOT NULL DEFAULT 'fact';
ALTER TABLE memories ADD COLUMN importance REAL NOT NULL DEFAULT 0.5;
ALTER TABLE memories ADD COLUMN related_to TEXT;

CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memory_type);
CREATE INDEX IF NOT EXISTS idx_memories_importance ON memories(importance);
CREATE INDEX IF NOT EXISTS idx_memories_accessed ON memories(accessed_at);
