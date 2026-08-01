-- Add Merkle hash-chain columns to audit_log
-- prev_hash: SHA-256 hex of the previous entry's hash (empty string for first)
-- entry_hash: SHA-256 hex of this entry's computed hash
ALTER TABLE audit_log ADD COLUMN prev_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE audit_log ADD COLUMN entry_hash TEXT NOT NULL DEFAULT '';
