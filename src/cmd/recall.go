package cmd

// recallCmd is the most-used command in the system and the one story 004 must
// port exactly: score = vector_distance_cos - RECALL_ALPHA * kw, lowest first.
// Identical recall@1/@5/MRR against the Python implementation is the acceptance
// criterion for the whole rewrite.
//
// It stays a PURE READER — no hit counters, no last-accessed column.
var recallCmd = &Command{
	Name:    "recall",
	Summary: "search memory, or load the session-start slots (--status/--baseline/--topics/--candidates)",
	Usage:   "supercharged-memory recall <query> [flags]",
	Story:   "004",
	Run:     notImplemented,
}
