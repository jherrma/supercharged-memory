package cmd

// statusCmd is the first thing every session runs. Its vocabulary is a contract:
// MISSING | EMPTY | DEGRADED n | ERROR ... | READY n, each of which
// CLAUDE.md.template tells the agent to react to differently. MISSING in
// particular must never be treated as lost data.
var statusCmd = &Command{
	Name:    "status",
	Summary: "report database health: MISSING | EMPTY | DEGRADED n | ERROR | READY n",
	Usage:   "supercharged-memory status",
	Story:   "004",
	Run:     notImplemented,
}
