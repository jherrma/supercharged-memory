-- Supercharged memory schema for tursodb (Turso 0.7.0). Vectors = bge-m3, 1024-dim.
-- ONE row per memory (no chunking). Hard caps via CHECK: memory_text <= 2000;
-- project/topic/source/model/embed_model <= 128. Keywords are NOT a column —
-- they are appended into memory_text by remember.py (embedded + LIKE-searchable).
-- (SQLite ignores declared VARCHAR(n) sizes — CHECK is how length is enforced.)
-- embed_model records which embedding model produced the vector; mixing models
-- across rows makes cosine meaningless, so the writer enforces one model per DB.
-- Idempotent create; full rebuild = DROP the two tables then run this.

CREATE TABLE
IF NOT EXISTS semantic_memory
(
  id             INTEGER PRIMARY KEY,
  created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  project        TEXT CHECK
(length
(project) <= 128),   -- NULL = global; tracking-tool work-item id
  topic          TEXT CHECK
(length
(topic) <= 128),     -- short headline/subject
  category       TEXT NOT NULL CHECK
(category IN
('baseline','user','feedback','project','reference','pattern')),
  -- 'pattern' rows are DERIVED, written only by a deep sleep pass: a recurrence or
  -- trend found across episodic events ("this class of bug came back 4x"). They
  -- carry the episodic ids they were derived from, so a later session can verify
  -- the claim instead of trusting it, and are revised via the supersede chain when
  -- the count changes. See instructions/DEEP-SLEEP.md.
  source         TEXT CHECK
(length
(source) <= 128),
  model          TEXT CHECK
(length
(model) <= 128),     -- agent model that wrote it
  embed_model    TEXT CHECK
(length
(embed_model) <= 128),
  memory_text    TEXT NOT NULL CHECK
(length
(memory_text) <= 2000),
  file_reference TEXT,
  embedding      F32_BLOB
(1024),
  superseded_by  INTEGER REFERENCES semantic_memory
(id),
  retired_at     TEXT     -- soft-delete (set by sleep.py --retire): obsolete,
                          -- no replacement. NULL = current. Never hard-deleted.
);

CREATE TABLE
IF NOT EXISTS episodic_memory
(
  id             INTEGER PRIMARY KEY,
  created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,  -- event time
  project        TEXT CHECK
(length
(project) <= 128),
  topic          TEXT CHECK
(length
(topic) <= 128),
  event_type     TEXT NOT NULL CHECK
(event_type IN
('project_start','bug_fix','feature_complete','decision','milestone','incident','note')),
  importance     TEXT NOT NULL CHECK
(importance IN
('routine','notable','major')),
  source         TEXT CHECK
(length
(source) <= 128),
  model          TEXT CHECK
(length
(model) <= 128),
  embed_model    TEXT CHECK
(length
(embed_model) <= 128),
  memory_text    TEXT NOT NULL CHECK
(length
(memory_text) <= 2000),
  file_reference TEXT,
  embedding      F32_BLOB
(1024),
  processed_at   TEXT     -- set by sleep.py once a sleep pass has sifted this
                          -- row (whether promoted to semantic or discarded as
                          -- a bare event with no lasting lesson). NULL = unprocessed.
);

CREATE INDEX
IF NOT EXISTS idx_sem_category ON semantic_memory
(category);
CREATE INDEX
IF NOT EXISTS idx_sem_current  ON semantic_memory
(superseded_by);
CREATE INDEX
IF NOT EXISTS idx_sem_project  ON semantic_memory
(project);
CREATE INDEX
IF NOT EXISTS idx_epi_type     ON episodic_memory
(event_type);
CREATE INDEX
IF NOT EXISTS idx_epi_project  ON episodic_memory
(project);
CREATE INDEX
IF NOT EXISTS idx_epi_processed ON episodic_memory
(processed_at);

-- Curated topic index, rebuilt wholesale by each sleep pass (sleep.py
-- --rebuild-topics: DELETE + re-INSERT, never accumulated). Deliberately
-- unlinked to semantic_memory/episodic_memory — a derived summary, not a
-- source of truth, so no FK and no embedding. Query by substring:
--   SELECT topic, keywords FROM topic_keywords WHERE topic LIKE '%x%' OR keywords LIKE '%x%';
CREATE TABLE
IF NOT EXISTS topic_keywords
(
  topic       TEXT NOT NULL PRIMARY KEY,
  keywords    TEXT NOT NULL,
  updated_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Coworkers: named personas with scoped memory + trust-gated autonomy.
-- No embedding column on coworkers/appraisals/memory_coworkers — none of
-- these are ever semantically searched, only looked up by id/coworker_id.
CREATE TABLE
IF NOT EXISTS coworkers
(
  id           INTEGER PRIMARY KEY,
  created_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  name         TEXT NOT NULL UNIQUE CHECK
(length
(name) <= 64),
  expertise    TEXT NOT NULL CHECK
(length
(expertise) <= 3000),
  personality  TEXT NOT NULL CHECK
(length
(personality) <= 3000),
  trust_level  TEXT NOT NULL DEFAULT 'supervised'
               CHECK
(trust_level IN
('supervised','trusted','autonomous')),
  active       INTEGER NOT NULL DEFAULT 1 CHECK
(active IN
(0,1))
);

-- Many-to-many: a memory can be relevant to zero (=global), one, or several
-- coworkers. No rows for a memory here = visible to everyone (unchanged
-- default behavior).
CREATE TABLE
IF NOT EXISTS memory_coworkers
(
  memory_table TEXT NOT NULL CHECK
(memory_table IN
('semantic','episodic')),
  memory_id    INTEGER NOT NULL,
  coworker_id  INTEGER NOT NULL REFERENCES coworkers
(id),
  PRIMARY KEY
(memory_table, memory_id, coworker_id)
);
CREATE INDEX
IF NOT EXISTS idx_mc_coworker ON memory_coworkers
(coworker_id);

-- One current appraisal per coworker (WHERE superseded_by IS NULL); history
-- preserved via the supersede chain, same pattern as semantic_memory revisions.
CREATE TABLE
IF NOT EXISTS appraisals
(
  id            INTEGER PRIMARY KEY,
  coworker_id   INTEGER NOT NULL REFERENCES coworkers
(id),
  created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  period_start  TEXT,              -- feedback window folded in; NULL = since coworker created
  trust_level   TEXT NOT NULL CHECK
(trust_level IN
('supervised','trusted','autonomous')), -- snapshot AT this review
  memory_text   TEXT NOT NULL CHECK
(length
(memory_text) <= 2000),
  superseded_by INTEGER REFERENCES appraisals
(id)
);
CREATE INDEX
IF NOT EXISTS idx_appr_coworker ON appraisals
(coworker_id);
CREATE INDEX
IF NOT EXISTS idx_appr_current  ON appraisals
(superseded_by);

-- ---------------------------------------------------------------------------
-- Recall evaluation (deep sleep D6). Lives in the DB so the .dump backup covers
-- it: the cases are AUTHORED, not derived, and cannot be regenerated from the
-- corpus — a query written from the row it should retrieve is the biased kind.
-- The query-embedding cache is deliberately NOT here; it is pure derived data.
CREATE TABLE IF NOT EXISTS eval_cases (
  id            TEXT PRIMARY KEY,                                   -- stable label, e.g. 'x01'
  created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  class         TEXT NOT NULL CHECK (length(class) <= 32),          -- exact-id, semantic, ...
  memory_table  TEXT NOT NULL CHECK (memory_table IN ('semantic','episodic')),
  query         TEXT NOT NULL CHECK (length(query) BETWEEN 1 AND 512),
  -- Targets as CSV, deliberately NOT a child table with an FK: a child table
  -- would be one more thing sleep.py --purge has to clean (see memory_coworkers),
  -- whereas a dangling id here is inert and is exactly what --validate looks for.
  expect_ids    TEXT NOT NULL CHECK (length(expect_ids) <= 256),
  -- created_at of each target AT AUTHORING TIME, same order as expect_ids.
  -- `id INTEGER PRIMARY KEY` is a rowid alias with no AUTOINCREMENT, so SQLite
  -- REUSES an id after the highest row is deleted — and D2 deletes rows. Without
  -- this stamp a purged-then-reused id still looks "live and current" while the
  -- case now points at an unrelated memory, corrupting the metric silently.
  expect_stamps TEXT NOT NULL CHECK (length(expect_stamps) <= 512),
  retired_at    TEXT                                                -- soft-delete, never DELETE
);
CREATE INDEX IF NOT EXISTS idx_eval_case_live ON eval_cases (retired_at);

-- One row per harness run; the baseline D6's regression check diffs against.
-- Derivable from nothing — a past corpus cannot be re-measured.
CREATE TABLE IF NOT EXISTS eval_runs (
  id           INTEGER PRIMARY KEY,
  ran_at       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  alpha        REAL NOT NULL,
  embed_model  TEXT NOT NULL CHECK (length(embed_model) <= 128),
  n_cases      INTEGER NOT NULL,
  r1           REAL NOT NULL,
  r5           REAL NOT NULL,
  mrr          REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_eval_runs_time ON eval_runs (ran_at);
