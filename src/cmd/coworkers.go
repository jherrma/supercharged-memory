package cmd

// coworkersCmd is writes only. Loading and listing a coworker stays ad-hoc SQL
// via the turso MCP server, per the instruction contract in CLAUDE.md.template.
// --retire sets active=0; it never deletes.
var coworkersCmd = &Command{
	Name:    "coworkers",
	Summary: "add, appraise, retire or reactivate a coworker persona (writes only)",
	Usage:   "supercharged-memory coworkers --add --name <n> ... | --appraise <name> ...",
	Story:   "006",
	Run:     notImplemented,
}
