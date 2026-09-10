package cmd

// backupCmd folds in scripts/supercharged-memory-backup.sh. That script was a
// second implementation of memlib rules in shell, and the two drifted on
// TURSO_VFS whitespace handling badly enough to kill the daily backup — one code
// path is the point of moving it here.
var backupCmd = &Command{
	Name:    "backup",
	Summary: "write a validated gzipped dump to BACKUP_DIR, with daily+weekly retention",
	Usage:   "supercharged-memory backup [flags]",
	Story:   "011",
	Run:     notImplemented,
}
