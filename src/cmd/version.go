package cmd

import (
	"fmt"
	"runtime/debug"
	"strings"
)

// Set with -ldflags at build time; see build.sh (story 002). The zero values are
// what a plain `go build` produces, and they say so rather than lying.
var (
	version = "dev"
	commit  = "unknown"
	date    = "unknown"
)

// tursoModule is the Turso SDK's module path. The version is read back out of the
// build info rather than hardcoded, so it cannot drift from what is linked in.
const tursoModule = "turso.tech/database/tursogo"

// VersionString reports the release version, the commit it was built from and the
// Turso SDK version. UPDATE.md compares the commit against the sync stamp in
// ~/.claude/CLAUDE.md to catch a repo that moved without its binary, so this is
// load-bearing output, not decoration.
func VersionString() string {
	var b strings.Builder
	fmt.Fprintf(&b, "supercharged-memory %s\n", version)
	fmt.Fprintf(&b, "  commit:    %s\n", commit)
	fmt.Fprintf(&b, "  built:     %s\n", date)
	fmt.Fprintf(&b, "  turso sdk: %s\n", tursoVersion())
	fmt.Fprintf(&b, "  go:        %s", goVersion())
	return b.String()
}

func tursoVersion() string {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return "unknown"
	}
	for _, dep := range info.Deps {
		if dep.Path == tursoModule {
			if dep.Replace != nil {
				return dep.Replace.Version + " (replaced)"
			}
			return dep.Version
		}
	}
	// Story 003 links the SDK. Until then this is the honest answer.
	return "not linked"
}

func goVersion() string {
	if info, ok := debug.ReadBuildInfo(); ok {
		return info.GoVersion
	}
	return "unknown"
}
