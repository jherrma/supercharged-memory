package cmd

// backfillCmd bulk-imports a directory, one file per memory. It shares the write
// path and every refusal with `remember` rather than keeping a second copy.
var backfillCmd = &Command{
	Name:    "backfill",
	Summary: "bulk-import a directory of files, one memory per file",
	Usage:   "supercharged-memory backfill --dir <path> [flags]",
	Story:   "008",
	Run:     notImplemented,
}
