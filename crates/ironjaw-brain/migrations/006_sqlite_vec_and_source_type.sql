-- Migration 006: sqlite-vec virtual table + source_type column
--
-- sqlite-vec: ANN vector search via virtual table (replaces O(n) cosine scan)
-- source_type: provenance tracking for memory deduplication

-- Vector index virtual table (requires sqlite-vec extension loaded)
-- 768 dimensions matches nomic-embed-text-v1.5 output
CREATE VIRTUAL TABLE IF NOT EXISTS vec_memories
USING vec0(embedding float[768], +memory_id INTEGER);

-- Source provenance column
ALTER TABLE memories ADD COLUMN source_type TEXT DEFAULT 'brain';
CREATE INDEX IF NOT EXISTS idx_memories_source_type ON memories(source_type);
