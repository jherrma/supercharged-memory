package cmd

// restoreCmd restores into a FRESH database file, never over a live one, and
// counts every table against the dump: a restore that is not counted is not a
// restore. That verification survives the rewrite even if the driver makes the
// statement-boundary splitting obsolete.
var restoreCmd = &Command{
	Name:    "restore",
	Summary: "restore a dump into a fresh database file, verifying row counts",
	Usage:   "supercharged-memory restore --out <path> [--dump <file>] [--force]",
	Story:   "010",
	Run:     notImplemented,
}
