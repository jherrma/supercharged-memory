-- Agent-foundations memory schema for tursodb (Turso 0.7.0). Vectors = bge-m3, 1024-dim.
-- ONE row per memory (no chunking). Hard caps via CHECK: memory_text <= 2000;
-- project/topic/source/model/embed_model <= 128. Keywords are NOT a column —
-- they are appended into memory_text by remember.py (embedded + LIKE-searchable).
-- (SQLite ignores declared VARCHAR(n) sizes — CHECK is how length is enforced.)
-- embed_model records which embedding model produced the vector; mixing models
-- across rows makes cosine meaningless, so the writer enforces one model per DB.
-- Idempotent create; full rebuild = DROP the two tables then run this.

CREATE TABLE IF NOT EXISTS semantic_memory (
  id             INTEGER PRIMARY KEY,
  created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  project        TEXT CHECK (length(project) <= 128),   -- NULL = global; tracking-tool work-item id
  topic          TEXT CHECK (length(topic) <= 128),     -- short headline/subject
  category       TEXT NOT NULL CHECK (category IN ('baseline','user','feedback','project','reference')),
  source         TEXT CHECK (length(source) <= 128),
  model          TEXT CHECK (length(model) <= 128),     -- agent model that wrote it
  embed_model    TEXT CHECK (length(embed_model) <= 128),
  memory_text    TEXT NOT NULL CHECK (length(memory_text) <= 2000),
  file_reference TEXT,
  embedding      F32_BLOB(1024),
  superseded_by  INTEGER REFERENCES semantic_memory(id)
);

CREATE TABLE IF NOT EXISTS episodic_memory (
  id             INTEGER PRIMARY KEY,
  created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,  -- event time
  project        TEXT CHECK (length(project) <= 128),
  topic          TEXT CHECK (length(topic) <= 128),
  event_type     TEXT NOT NULL CHECK (event_type IN ('project_start','bug_fix','feature_complete','decision','milestone','incident','note')),
  importance     TEXT NOT NULL CHECK (importance IN ('routine','notable','major')),
  source         TEXT CHECK (length(source) <= 128),
  model          TEXT CHECK (length(model) <= 128),
  embed_model    TEXT CHECK (length(embed_model) <= 128),
  memory_text    TEXT NOT NULL CHECK (length(memory_text) <= 2000),
  file_reference TEXT,
  embedding      F32_BLOB(1024)
);

CREATE INDEX IF NOT EXISTS idx_sem_category ON semantic_memory(category);
CREATE INDEX IF NOT EXISTS idx_sem_current  ON semantic_memory(superseded_by);
CREATE INDEX IF NOT EXISTS idx_sem_project  ON semantic_memory(project);
CREATE INDEX IF NOT EXISTS idx_epi_type     ON episodic_memory(event_type);
CREATE INDEX IF NOT EXISTS idx_epi_project  ON episodic_memory(project);

-- Coworkers: named personas with scoped memory + trust-gated autonomy.
-- No embedding column on coworkers/appraisals/memory_coworkers — none of
-- these are ever semantically searched, only looked up by id/coworker_id.
CREATE TABLE IF NOT EXISTS coworkers (
  id           INTEGER PRIMARY KEY,
  created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  name         TEXT NOT NULL UNIQUE CHECK (length(name) <= 64),
  expertise    TEXT NOT NULL CHECK (length(expertise) <= 256),
  personality  TEXT NOT NULL CHECK (length(personality) <= 1000),
  trust_level  TEXT NOT NULL DEFAULT 'supervised'
               CHECK (trust_level IN ('supervised','trusted','autonomous')),
  active       INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1))
);

-- Many-to-many: a memory can be relevant to zero (=global), one, or several
-- coworkers. No rows for a memory here = visible to everyone (unchanged
-- default behavior).
CREATE TABLE IF NOT EXISTS memory_coworkers (
  memory_table TEXT NOT NULL CHECK (memory_table IN ('semantic','episodic')),
  memory_id    INTEGER NOT NULL,
  coworker_id  INTEGER NOT NULL REFERENCES coworkers(id),
  PRIMARY KEY (memory_table, memory_id, coworker_id)
);
CREATE INDEX IF NOT EXISTS idx_mc_coworker ON memory_coworkers(coworker_id);

-- One current appraisal per coworker (WHERE superseded_by IS NULL); history
-- preserved via the supersede chain, same pattern as semantic_memory revisions.
CREATE TABLE IF NOT EXISTS appraisals (
  id            INTEGER PRIMARY KEY,
  coworker_id   INTEGER NOT NULL REFERENCES coworkers(id),
  created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  period_start  TEXT,              -- feedback window folded in; NULL = since coworker created
  trust_level   TEXT NOT NULL CHECK (trust_level IN ('supervised','trusted','autonomous')), -- snapshot AT this review
  memory_text   TEXT NOT NULL CHECK (length(memory_text) <= 2000),
  superseded_by INTEGER REFERENCES appraisals(id)
);
CREATE INDEX IF NOT EXISTS idx_appr_coworker ON appraisals(coworker_id);
CREATE INDEX IF NOT EXISTS idx_appr_current  ON appraisals(superseded_by);
