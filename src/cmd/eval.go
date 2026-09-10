package cmd

// evalCmd is not a side experiment: deep sleep D6 runs it, and it is how story
// 004 proves the Go ranking matches the Python one. eval_cases are AUTHORED and
// cannot be regenerated — treat those tables as data, never as cache.
var evalCmd = &Command{
	Name:    "eval",
	Summary: "recall-quality harness: validate the eval set, report metrics, sweep RECALL_ALPHA",
	Usage:   "supercharged-memory eval --validate | --report | --sweep <alphas>",
	Story:   "014",
	Run:     notImplemented,
}
