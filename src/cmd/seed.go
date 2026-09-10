package cmd

// seedCmd exists for a brand-new database. Story 009 has to decide whether it
// survives at all: its Python interface was "edit the list inside the script",
// which a compiled binary cannot offer.
var seedCmd = &Command{
	Name:    "seed",
	Summary: "seed a fresh database with starter memories",
	Usage:   "supercharged-memory seed [flags]",
	Story:   "009",
	Run:     notImplemented,
}
