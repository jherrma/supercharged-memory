package cmd

// rememberCmd owns every mechanical guard that keeps the corpus clean: the
// near-duplicate refusal, the 2000-char cap, the one-embedding-model-per-DB rule,
// the baseline confirmation, the --created-at format. A permissive port degrades
// memory quality invisibly, so each refusal ships with a test (story 005).
var rememberCmd = &Command{
	Name:    "remember",
	Summary: "store one memory, or supersede existing ones",
	Usage:   "supercharged-memory remember --table <t> --topic <t> --text <...> [flags]",
	Story:   "005",
	Run:     notImplemented,
}
