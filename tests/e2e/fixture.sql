-- The fixed corpus every end-to-end run and every benchmark run starts from.
--
-- Applied on top of schema.sql. Plain SQL only (no vector functions), so the
-- Rust harness (turso) and the benchmark driver (Python's sqlite3) build the
-- same database from it. Embeddings are not written here: every row whose
-- embed_model is set gets the deterministic fake-Ollama vector of its own
-- memory_text when the database is built, and a row with embed_model NULL is
-- left without one. The "embedded" variant of the fixture gives every row a
-- vector.
--
-- Every date lies in 2025, so a timestamp written during a run is never
-- mistaken for a fixture value. Every IDF scope a comparison searches holds at
-- least five rows.
--
-- No real person, ticket or machine is named anywhere in here.

INSERT INTO semantic_memory
  (id, created_at, updated_at, project, topic, category, source, model, embed_model, memory_text, superseded_by, retired_at)
VALUES
  (1, '2025-03-01 09:00:00', '2025-03-01 09:00:00', NULL, 'commit-policy', 'baseline', 'user-stated', 'fixture-model', 'bge-m3',
   'Never commit or push without an explicit request; leave every change in the working tree.

Keywords: git, commit, push, policy', NULL, NULL),
  (2, '2025-03-02 09:00:00', '2025-03-02 09:00:00', NULL, 'pii-rule', 'baseline', 'user-stated', 'fixture-model', 'bge-m3',
   'Never store personal data in memory; store ticket ids and role labels instead.

Keywords: pii, privacy, anonymize', NULL, NULL),
  (3, '2025-04-10 10:00:00', '2025-04-10 10:00:00', NULL, 'shell', 'user', 'user-stated', 'fixture-model', 'bge-m3',
   'The shell is zsh, where an unquoted variable does not word-split; put multi-word commands into a bash script.

Keywords: zsh, word splitting, bash', NULL, NULL),
  (4, '2025-05-01 10:00:00', '2025-06-01 10:00:00', NULL, 'backup-retention', 'project', 'self-observed', 'fixture-model', 'bge-m3',
   'Backups keep two daily dumps and nothing weekly.', 5, NULL),
  (5, '2025-06-01 10:00:00', '2025-06-01 10:00:00', NULL, 'backup-retention', 'project', 'self-observed', 'fixture-model', 'bge-m3',
   'Backups keep three daily dumps and four weekly dumps; the weekly copy is taken on Mondays.

Keywords: backup, retention, weekly, daily', NULL, NULL),
  (6, '2025-06-15 10:00:00', '2025-07-01 10:00:00', NULL, 'old-ollama-port', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'Ollama listened on port 11435 during the migration rehearsal.', NULL, '2025-07-01 10:00:00'),
  (7, '2025-07-02 11:00:00', '2025-07-02 11:00:00', 'proj-alpha', 'turso-wal', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'Every tursodb opener needs the multiprocess wal flag, or it takes an exclusive lock and blocks every reader.

Keywords: turso, wal, lock, multiprocess', NULL, NULL),
  (8, '2025-07-03 11:00:00', '2025-07-03 11:00:00', 'proj-alpha', 'turso-busy', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'A contended tursodb write prints database is busy with no prefix; retry it with a backoff.

Keywords: turso, busy, retry, backoff', NULL, NULL),
  (9, '2025-07-04 11:00:00', '2025-07-04 11:00:00', 'proj-alpha', 'ollama-model', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'The embedding model is bge-m3 with 1024 dimensions, served by Ollama on port 11434.

Keywords: ollama, embedding, dimensions', NULL, NULL),
  (10, '2025-07-05 11:00:00', '2025-07-05 11:00:00', 'proj-alpha', 'ollama-model-local', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'The embedding model is bge-m3 with 1024 dimensions, served locally by Ollama on port 11434.

Keywords: ollama, embedding, dimensions', NULL, NULL),
  (11, '2025-07-06 11:00:00', '2025-07-06 11:00:00', 'proj-alpha', 'turso-checkpoint', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'A truncating wal checkpoint leaves the wal file empty, so the main database file can be moved alone.', NULL, NULL),
  (12, '2025-08-01 12:00:00', '2025-08-01 12:00:00', 'proj-beta', 'clippy-gate', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'Before handing work over, run cargo clippy on all targets and treat every warning as a failure of the gate.', NULL, NULL),
  (13, '2025-08-02 12:00:00', '2025-08-02 12:00:00', 'proj-beta', 'fmt-gate', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'Before handing work over, run cargo fmt check on all targets and treat every diff as a failure of the gate.', NULL, NULL),
  (14, '2025-08-03 12:00:00', '2025-08-03 12:00:00', 'proj-beta', 'test-gate', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'Before handing work over, run cargo test on all targets and treat every failure as a failure of the gate.', NULL, NULL),
  (15, '2025-08-04 12:00:00', '2025-08-04 12:00:00', 'proj-beta', 'restore-split', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'Never pipe a dump into tursodb: it splits on line boundaries and loses rows holding newlines. Use scripts/restore.py --out instead.', NULL, NULL),
  (16, '2025-08-05 12:00:00', '2025-08-05 12:00:00', 'proj-beta', 'docs-gate', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'The docs are tested against the command line surface, so renaming a flag fails the runbook tests.', NULL, NULL),
  (17, '2025-08-10 12:00:00', '2025-08-10 12:00:00', NULL, 'review-tests', 'feedback', 'user-stated', 'fixture-model', 'bge-m3',
   'Every refusal needs a test that asserts the exit code and the refused prefix.', NULL, NULL),
  (18, '2025-08-11 12:00:00', '2025-08-11 12:00:00', NULL, 'commit-style', 'feedback', 'user-stated', 'fixture-model', 'bge-m3',
   'Commit messages are short and in the imperative mood.', NULL, NULL),
  (19, '2025-08-12 12:00:00', '2025-08-12 12:00:00', NULL, 'db-path-env', 'reference', 'self-observed', 'fixture-model', 'bge-m3',
   'SUPERCHARGED_MEMORY_TURSO_PATH names the database, and tursodb 0.7.2 is the version in use.', NULL, NULL),
  (20, '2025-08-13 12:00:00', '2025-08-13 12:00:00', NULL, 'lock-pattern', 'pattern', 'self-observed', 'fixture-model', 'bge-m3',
   'Lock contention recurs whenever two sessions write at once; the retry loop absorbs it.', NULL, NULL),
  (21, '2025-09-01 12:00:00', '2025-09-01 12:00:00', NULL, 'python-runtime', 'reference', 'self-observed', 'fixture-model', NULL,
   'The old scripts run under python3 and need tursodb on the search path.', NULL, NULL),
  (22, '2025-09-02 12:00:00', '2025-09-02 12:00:00', NULL, 'backup-timer', 'reference', 'self-observed', 'fixture-model', NULL,
   'The backup job runs daily at noon from a user timer.', NULL, NULL);

INSERT INTO episodic_memory
  (id, created_at, project, topic, event_type, importance, source, model, embed_model, memory_text, processed_at)
VALUES
  (1, '2025-03-03 09:00:00', 'proj-alpha', 'rewrite-start', 'project_start', 'notable', 'self-observed', 'fixture-model', 'bge-m3',
   'Started the native command line rewrite of the memory scripts.', '2025-04-01 00:00:00'),
  (2, '2025-04-01 09:00:00', 'proj-alpha', 'busy-retry', 'bug_fix', 'major', 'self-observed', 'fixture-model', 'bge-m3',
   'Fixed lost writes: the busy check never fired, because contention prints no prefix. Root cause found and verified.', '2025-04-02 00:00:00'),
  (3, '2025-05-05 09:00:00', 'proj-alpha', 'backup-format', 'decision', 'notable', 'self-observed', 'fixture-model', 'bge-m3',
   'Decided that the backup writes the dump format natively instead of shelling out to tursodb.', NULL),
  (4, '2025-06-10 09:00:00', 'proj-alpha', 'restore-counted', 'milestone', 'major', 'self-observed', 'fixture-model', 'bge-m3',
   'Restore now counts every table against the dump before the file takes its name.', NULL),
  (5, '2025-07-15 09:00:00', 'proj-beta', 'wrong-db-path', 'incident', 'major', 'self-observed', 'fixture-model', 'bge-m3',
   'A session started on the wrong database path and reported MISSING; the candidates check found the real database.', NULL),
  (6, '2025-08-20 09:00:00', 'proj-beta', 'clippy-clean', 'note', 'routine', 'self-observed', 'fixture-model', 'bge-m3',
   'Clippy reported zero warnings across all targets.', NULL),
  (7, '2025-09-01 09:00:00', 'proj-alpha', 'embed-missing', 'feature_complete', 'notable', 'self-observed', 'fixture-model', 'bge-m3',
   'Embed missing now fills the vectors of rows stored without one, batch by batch.', NULL),
  (8, '2025-09-15 09:00:00', NULL, 'zsh-split', 'bug_fix', 'routine', 'self-observed', 'fixture-model', 'bge-m3',
   'A command failed in zsh because the variable did not word-split; it moved into a bash script.', NULL),
  (9, '2025-10-01 09:00:00', NULL, 'stored-degraded', 'note', 'routine', 'self-observed', 'fixture-model', NULL,
   'Stored this event while Ollama was down.', NULL),
  (10, '2025-10-02 09:00:00', 'proj-alpha', 'eval-sweep', 'note', 'routine', 'self-observed', 'fixture-model', 'bge-m3',
   'Swept the recall alpha; the configured value sat inside the plateau.', NULL);

INSERT INTO topic_keywords (topic, keywords, updated_at) VALUES
  ('turso', 'wal, busy, lock, checkpoint', '2025-09-01 00:00:00'),
  ('backup', 'retention, restore, dump', '2025-09-02 00:00:00'),
  ('rust', 'clippy, fmt, cargo test', '2025-09-03 00:00:00');

INSERT INTO coworkers (id, created_at, updated_at, name, expertise, personality, trust_level, active) VALUES
  (1, '2025-02-01 08:00:00', '2025-06-01 08:00:00', 'ada', 'storage engines and recovery', 'terse, asks for the failing test first', 'trusted', 1),
  (2, '2025-02-02 08:00:00', '2025-02-02 08:00:00', 'bob', 'shell tooling', 'patient, prefers small steps', 'supervised', 0);

INSERT INTO appraisals (id, coworker_id, created_at, period_start, trust_level, memory_text, superseded_by) VALUES
  (1, 1, '2025-03-01 08:00:00', '2025-02-01 08:00:00', 'supervised', 'First review: careful, needs supervision on schema changes.', 2),
  (2, 1, '2025-06-01 08:00:00', '2025-03-01 08:00:00', 'trusted', 'Second review: reliable on recovery work.', NULL);

INSERT INTO memory_coworkers (memory_table, memory_id, coworker_id) VALUES
  ('semantic', 17, 1),
  ('semantic', 18, 2),
  ('episodic', 3, 1);

INSERT INTO instructions (id, created_at, updated_at, project, topic, source, model, embed_model, memory_text, retired_at) VALUES
  (1, '2025-05-01 08:00:00', '2025-05-01 08:00:00', NULL, 'release-checklist', 'user-stated', 'fixture-model', 'bge-m3',
   'Before a release, bump the version, build natively, run the full test suite and write the notes.', NULL),
  (2, '2025-05-02 08:00:00', '2025-05-02 08:00:00', NULL, 'review-style', 'user-stated', 'fixture-model', 'bge-m3',
   'Review findings go in order of severity, each with a file path and a one-line fix.', NULL),
  (3, '2025-05-03 08:00:00', '2025-05-03 08:00:00', NULL, 'commit-message', 'user-stated', 'fixture-model', 'bge-m3',
   'Commit subjects are imperative and at most seventy characters long.', NULL),
  (4, '2025-05-04 08:00:00', '2025-05-05 08:00:00', NULL, 'review-style', 'user-stated', 'fixture-model', 'bge-m3',
   'Review findings go in file order.', '2025-05-05 08:00:00'),
  (5, '2025-05-06 08:00:00', '2025-05-06 08:00:00', 'proj-alpha', 'bench-report', 'user-stated', 'fixture-model', 'bge-m3',
   'Benchmark reports carry the host, the sample counts, the median and the p95.', NULL),
  (6, '2025-05-07 08:00:00', '2025-05-07 08:00:00', NULL, 'standup', 'user-stated', 'fixture-model', NULL,
   'The standup summary lists yesterday, today and the blockers.', NULL),
  (7, '2025-05-08 08:00:00', '2025-05-08 08:00:00', NULL, 'handover', 'user-stated', 'fixture-model', 'bge-m3',
   'A handover lists the files touched, the open questions and the verification counts.', NULL);

INSERT INTO eval_cases (id, created_at, class, memory_table, query, expect_ids, expect_stamps) VALUES
  ('x01', '2025-09-10 08:00:00', 'exact-id', 'semantic', 'multiprocess wal flag exclusive lock', '7', '2025-07-02 11:00:00'),
  ('x02', '2025-09-10 08:00:00', 'exact-id', 'semantic', 'backup retention weekly dumps', '5', '2025-06-01 10:00:00'),
  ('x03', '2025-09-10 08:00:00', 'paraphrase', 'semantic', 'which shell splits words', '3', '2025-04-10 10:00:00'),
  ('x04', '2025-09-10 08:00:00', 'paraphrase', 'episodic', 'why did the busy check never fire', '2', '2025-04-01 09:00:00'),
  ('x05', '2025-09-10 08:00:00', 'exact-id', 'semantic', 'how many daily dumps were kept', '4', '2025-05-01 10:00:00'),
  ('x06', '2025-09-10 08:00:00', 'paraphrase', 'episodic', 'session on the wrong database path', '5', '2025-07-15 09:00:00');

INSERT INTO eval_runs (id, ran_at, alpha, embed_model, n_cases, r1, r5, mrr) VALUES
  (1, '2025-09-30 12:00:00', 0.15, 'bge-m3', 6, 0.5, 0.75, 0.625);
