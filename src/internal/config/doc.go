// Package config resolves the environment every other package reads.
//
// Filled in by story 003, keeping today's names and defaults unchanged:
// SUPERCHARGED_MEMORY_TURSO_PATH (${XDG_DATA_HOME:-~/.local/share}/turso/
// supercharged-memory.db), BACKUP_DIR (<repo>/backups), OLLAMA_URL, EMBED_MODEL,
// RECALL_ALPHA, TURSO_VFS.
//
// One rule implemented once: the shell backup script re-implemented TURSO_VFS
// handling and drifted from memlib.py on whitespace, which killed the daily
// backup with `no such VFS:  none`. Every caller resolves config here.
package config
