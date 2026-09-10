//! Environment resolution: the one place every other module reads its settings.
//!
//! Filled in by story 003, keeping today's names and defaults unchanged:
//! `SUPERCHARGED_MEMORY_TURSO_PATH` (default
//! `${XDG_DATA_HOME:-~/.local/share}/turso/supercharged-memory.db`), `BACKUP_DIR`
//! (default `<repo>/backups`), `OLLAMA_URL`, `EMBED_MODEL`, `RECALL_ALPHA`,
//! `TURSO_VFS`.
//!
//! One rule implemented once. The shell backup script re-implemented `TURSO_VFS`
//! handling and drifted from `memlib.py` on whitespace, so `TURSO_VFS=' none '`
//! left every Python script working and killed the daily backup with
//! `no such VFS:  none`. Every caller resolves configuration here, and the
//! resolution matrix (unset, empty, whitespace-only, mixed case, bogus) is a test.
