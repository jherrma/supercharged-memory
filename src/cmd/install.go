package cmd

// installCmd renders CLAUDE.md.template into ~/.claude/CLAUDE.md between the
// managed markers. It carries more state than its size suggests: it reads the
// existing block BEFORE it defaults, recovers EPISODIC_MODE and BASE_PATH from
// it, refuses a BASE_PATH that disagrees, and writes the sync stamp UPDATE.md is
// built on.
var installCmd = &Command{
	Name:    "install",
	Summary: "render CLAUDE.md.template into ~/.claude/CLAUDE.md between the managed markers",
	Usage:   "supercharged-memory install [flags]",
	Story:   "013",
	Run:     notImplemented,
}
