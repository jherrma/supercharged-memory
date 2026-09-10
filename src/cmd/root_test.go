package cmd

import (
	"errors"
	"flag"
	"testing"
)

// The exit-code and refusal conventions are a contract the runbooks are written
// against: `refused:` means nothing happened. Later stories add guards that rely
// on it, so lock it here rather than rediscovering it per command.

func TestRefusalFormatsWithThePrefixTheRunbooksExpect(t *testing.T) {
	err := Refusef("memory is %d chars; hard cap %d", 2116, 2000)
	if got, want := err.Error(), "refused: memory is 2116 chars; hard cap 2000"; got != want {
		t.Errorf("got %q, want %q", got, want)
	}
	if !isRefusal(err) {
		t.Error("Refusef must produce an error Main recognises as a refusal")
	}
}

func TestExitCodes(t *testing.T) {
	probe := &Command{Name: "probe", Summary: "test", Usage: "probe", Story: "001"}
	commands = append(commands, probe)
	t.Cleanup(func() { commands = commands[:len(commands)-1] })

	cases := []struct {
		name string
		run  func(*Command, []string) error
		args []string
		want int
	}{
		{"success", func(*Command, []string) error { return nil }, []string{"probe"}, exitOK},
		{"refusal", func(*Command, []string) error { return Refusef("no") }, []string{"probe"}, exitRefused},
		{"error", func(*Command, []string) error { return errors.New("boom") }, []string{"probe"}, exitError},
		{"help is not a failure", func(*Command, []string) error { return flag.ErrHelp }, []string{"probe"}, exitOK},
		{"not implemented", notImplemented, []string{"probe"}, exitError},
		{"unknown flag", notImplemented, []string{"probe", "--nope"}, exitUsage},
		{"unknown command", nil, []string{"frobnicate"}, exitUsage},
		{"no args", nil, nil, exitUsage},
		{"version", nil, []string{"--version"}, exitOK},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			probe.Run = tc.run
			if got := Main(tc.args); got != tc.want {
				t.Errorf("Main(%q) = %d, want %d", tc.args, got, tc.want)
			}
		})
	}
}

// Every subcommand the rewrite promises must exist, so a runbook rewritten
// against this surface cannot reference a command that was never wired up.
func TestEverySubcommandIsRegistered(t *testing.T) {
	want := []string{
		"status", "recall", "remember", "coworkers", "sleep", "backfill",
		"seed", "restore", "backup", "find-existing-memory", "install", "eval",
	}
	for _, name := range want {
		c := lookup(name)
		if c == nil {
			t.Errorf("subcommand %q is not registered", name)
			continue
		}
		if c.Summary == "" || c.Usage == "" || c.Story == "" {
			t.Errorf("subcommand %q is missing Summary, Usage or Story", name)
		}
	}
}
