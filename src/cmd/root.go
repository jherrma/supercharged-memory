// Package cmd holds the CLI surface: one file per subcommand, plus this
// dispatcher. Command implementations live under internal/ — a file in this
// package parses flags and reports, it does not own logic.
package cmd

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"text/tabwriter"
)

// Exit codes. Anything non-zero means the command did not do what was asked;
// exitRefused specifically means NOTHING HAPPENED, which is the distinction the
// runbooks are written against.
const (
	exitOK      = 0
	exitError   = 1
	exitRefused = 2
	exitUsage   = 64 // sysexits.h EX_USAGE: the invocation itself was wrong
)

// Refusal is a guard rail firing: the command declined to act and changed
// nothing. It prints as `refused: <reason>`, the same wording the Python scripts
// used, because CLAUDE.md.template teaches the agent to read that word.
type Refusal struct{ Reason string }

func (r *Refusal) Error() string { return "refused: " + r.Reason }

// Refusef builds a Refusal. Use it for every mechanical guard — a near-duplicate
// memory, a missing backup before a purge, an unknown trust level.
func Refusef(format string, args ...any) error {
	return &Refusal{Reason: fmt.Sprintf(format, args...)}
}

// Command is one subcommand. Story names the story that fills it in, so an
// unimplemented command can say where its implementation is being tracked.
type Command struct {
	Name    string
	Summary string
	Usage   string
	Story   string
	Run     func(c *Command, args []string) error
}

// FlagSet returns a flag set that REJECTS unknown flags rather than ignoring
// them. install-claude-md.sh learned this the hard way; a typo'd flag that is
// silently dropped turns a refusal into a no-op that looks like success.
func (c *Command) FlagSet() *flag.FlagSet {
	fs := flag.NewFlagSet(c.Name, flag.ContinueOnError)
	fs.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s\n\n%s\n", c.Usage, c.Summary)
		var count int
		fs.VisitAll(func(*flag.Flag) { count++ })
		if count > 0 {
			fmt.Fprintln(os.Stderr, "\nflags:")
			fs.PrintDefaults()
		}
	}
	return fs
}

// errNotImplemented is what every story-001 stub returns.
var errNotImplemented = errors.New("not implemented")

// flagError wraps a flag-parsing failure. The flag package has ALREADY printed
// the message and the usage text by the time Parse returns, so Main must not
// print it a second time -- it only maps this to the usage exit code.
type flagError struct{ err error }

func (e *flagError) Error() string { return e.err.Error() }
func (e *flagError) Unwrap() error { return e.err }

// notImplemented is the placeholder Run for a subcommand whose story has not
// landed. It still parses flags, so `--help` works and an unknown flag is
// rejected before the stub reports itself.
func notImplemented(c *Command, args []string) error {
	fs := c.FlagSet()
	if err := fs.Parse(args); err != nil {
		return &flagError{err}
	}
	return fmt.Errorf("%w: %s is filled in by story %s", errNotImplemented, c.Name, c.Story)
}

// commands is the full surface. Every script the Python implementation exposed
// has a home here; see stories/17-go-rewrite/STORIES.md.
var commands = []*Command{
	statusCmd,
	recallCmd,
	rememberCmd,
	coworkersCmd,
	sleepCmd,
	backfillCmd,
	seedCmd,
	restoreCmd,
	backupCmd,
	findExistingMemoryCmd,
	installCmd,
	evalCmd,
}

func lookup(name string) *Command {
	for _, c := range commands {
		if c.Name == name {
			return c
		}
	}
	return nil
}

func usage(w io.Writer) {
	fmt.Fprintf(w, "supercharged-memory — long-term memory for AI coding agents\n\n")
	fmt.Fprintf(w, "usage: supercharged-memory <command> [flags]\n\ncommands:\n")
	tw := tabwriter.NewWriter(w, 0, 0, 2, ' ', 0)
	for _, c := range commands {
		fmt.Fprintf(tw, "  %s\t%s\n", c.Name, c.Summary)
	}
	tw.Flush()
	fmt.Fprintf(w, "\nRun `supercharged-memory <command> --help` for a command's flags.\n")
	fmt.Fprintf(w, "Use `--version` for version, build commit and Turso SDK version.\n")
}

// Main dispatches one invocation and returns the process exit code.
func Main(args []string) int {
	if len(args) == 0 {
		usage(os.Stdout)
		return exitUsage
	}

	switch args[0] {
	case "--version", "-version", "version":
		fmt.Println(VersionString())
		return exitOK
	case "--help", "-help", "-h", "help":
		usage(os.Stdout)
		return exitOK
	}

	if args[0][0] == '-' {
		fmt.Fprintf(os.Stderr, "unknown flag: %s\n\n", args[0])
		usage(os.Stderr)
		return exitUsage
	}

	c := lookup(args[0])
	if c == nil {
		fmt.Fprintf(os.Stderr, "unknown command: %s\n\n", args[0])
		usage(os.Stderr)
		return exitUsage
	}

	switch err := c.Run(c, args[1:]); {
	case err == nil:
		return exitOK
	case errors.Is(err, flag.ErrHelp):
		// FlagSet.Usage already printed; --help is not a failure.
		return exitOK
	case isFlagError(err):
		// The flag package printed the message and the usage text already.
		return exitUsage
	case isRefusal(err):
		fmt.Fprintln(os.Stderr, err)
		return exitRefused
	default:
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		return exitError
	}
}

func isRefusal(err error) bool {
	var r *Refusal
	return errors.As(err, &r)
}

func isFlagError(err error) bool {
	var f *flagError
	return errors.As(err, &f)
}
