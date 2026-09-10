package cmd

// findExistingMemoryCmd is the read-only probe SETUP.md Step 7 gates on. Its
// defining property is that it needs NOTHING — no database, no Ollama — because
// it runs before either is proven working. Every skipped path must land in a
// reported field; a skip in no field reads as "you have no memory there".
var findExistingMemoryCmd = &Command{
	Name:    "find-existing-memory",
	Summary: "probe for file-based Claude memory to migrate (read-only, no DB, no Ollama)",
	Usage:   "supercharged-memory find-existing-memory [--quiet]",
	Story:   "012",
	Run:     notImplemented,
}
