package cmd

// sleepCmd is deliberately dumb: judgment stays with the LLM driving
// instructions/SLEEP.md, and this command supplies atomic writes and mechanical
// analysis. --purge is the single hard-delete path in the whole system and keeps
// all four of its refusals.
var sleepCmd = &Command{
	Name:    "sleep",
	Summary: "consolidation primitives: mark-processed, retire, rebuild-topics, cluster, purge",
	Usage:   "supercharged-memory sleep --<primitive> [flags]",
	Story:   "007",
	Run:     notImplemented,
}
