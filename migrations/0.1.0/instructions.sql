-- Instructions: directives the user wants followed on demand. Never loaded at
-- session start; looked up when the user asks for one. Mirrors semantic_memory
-- without the supersede chain and without category: a revision retires the old
-- row and inserts a new one.
CREATE TABLE IF NOT EXISTS instructions (
  id             INTEGER PRIMARY KEY,
  created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  project        TEXT CHECK (length(project) <= 128),  -- NULL = global; work-item id
  topic          TEXT NOT NULL CHECK (length(topic) BETWEEN 1 AND 128), -- the name the user says
  source         TEXT CHECK (length(source) <= 128),
  model          TEXT CHECK (length(model) <= 128),
  embed_model    TEXT CHECK (length(embed_model) <= 128),
  memory_text    TEXT NOT NULL CHECK (length(memory_text) <= 2000),
  file_reference TEXT,
  embedding      F32_BLOB(1024),
  retired_at     TEXT   -- soft-delete; NULL = current. Never hard-deleted.
);
CREATE INDEX IF NOT EXISTS idx_instr_retired ON instructions (retired_at);
