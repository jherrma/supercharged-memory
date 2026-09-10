// Command supercharged-memory is the single binary behind the long-term memory
// system: every operation the Python scripts used to provide, plus the schedulers
// and installers that drive them.
//
// It replaces a set of scripts that shelled out to the tursodb CLI and parsed its
// text output; see issue #17. Story 001 established this skeleton — the
// subcommands are wired up and documented here, and each one is filled in by its
// own story.
package main

import (
	"os"

	"github.com/jherrma/supercharged-memory/cmd"
)

func main() {
	os.Exit(cmd.Main(os.Args[1:]))
}
