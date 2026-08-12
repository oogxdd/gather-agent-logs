package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	tea "charm.land/bubbletea/v2"
	"golang.org/x/term"

	"github.com/oogxdd/gather-agent-logs/go/internal/resume"
	"github.com/oogxdd/gather-agent-logs/go/internal/session"
	"github.com/oogxdd/gather-agent-logs/go/internal/tui"
)

type options struct {
	agent     string
	sort      string
	home      string
	codexDir  string
	claudeDir string
	list      bool
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func run(args []string) error {
	opts, err := parseOptions(args)
	if errors.Is(err, flag.ErrHelp) {
		return nil
	}
	if err != nil {
		return err
	}

	sortMode, err := session.ParseSortMode(opts.sort)
	if err != nil {
		return err
	}
	includeCodex, includeClaude, err := agentSelection(opts.agent)
	if err != nil {
		return err
	}
	codexDir, claudeDir := sessionDirectories(opts)
	result := session.Discover(session.Options{
		CodexDir:      codexDir,
		ClaudeDir:     claudeDir,
		IncludeCodex:  includeCodex,
		IncludeClaude: includeClaude,
	})
	session.Sort(result.Sessions, sortMode)

	if opts.list {
		printSessions(result.Sessions)
		printSkippedWarning(result.Skipped)
		return nil
	}
	if !term.IsTerminal(int(os.Stdin.Fd())) || !term.IsTerminal(int(os.Stdout.Fd())) {
		return errors.New("the picker needs an interactive terminal; use --list for plain output")
	}
	if len(result.Sessions) == 0 {
		return errors.New("no Codex or Claude Code sessions found")
	}

	final, err := tea.NewProgram(tui.New(result.Sessions, result.Skipped, sortMode)).Run()
	if err != nil {
		return fmt.Errorf("run picker: %w", err)
	}
	model, ok := final.(tui.Model)
	if !ok {
		return fmt.Errorf("picker returned unexpected model %T", final)
	}
	chosen, ok := model.Selection()
	if !ok {
		return nil
	}
	spec, err := resume.For(chosen)
	if err != nil {
		return err
	}
	return resume.Run(spec)
}

func parseOptions(args []string) (options, error) {
	var opts options
	flags := flag.NewFlagSet("agent-resume-go", flag.ContinueOnError)
	flags.SetOutput(os.Stderr)
	flags.StringVar(&opts.agent, "agent", "all", "limit sessions to all, codex, or claude")
	flags.StringVar(&opts.sort, "sort", "updated", "initial sort order: updated or created")
	flags.StringVar(&opts.home, "home", "", "scan another home directory")
	flags.StringVar(&opts.codexDir, "codex-dir", "", "override the Codex sessions directory")
	flags.StringVar(&opts.claudeDir, "claude-dir", "", "override the Claude projects directory")
	flags.BoolVar(&opts.list, "list", false, "print discovered sessions without opening the TUI")
	flags.Usage = func() {
		fmt.Fprintln(flags.Output(), "agent-resume-go finds and resumes local Codex and Claude Code sessions.")
		fmt.Fprintln(flags.Output())
		fmt.Fprintln(flags.Output(), "Usage: agent-resume-go [OPTIONS]")
		fmt.Fprintln(flags.Output())
		flags.PrintDefaults()
	}
	err := flags.Parse(args)
	if err == nil && flags.NArg() != 0 {
		return options{}, fmt.Errorf("unexpected positional argument %q", flags.Arg(0))
	}
	return opts, err
}

func agentSelection(value string) (bool, bool, error) {
	switch strings.ToLower(value) {
	case "all":
		return true, true, nil
	case "codex":
		return true, false, nil
	case "claude":
		return false, true, nil
	default:
		return false, false, fmt.Errorf("invalid agent %q (want all, codex, or claude)", value)
	}
}

func sessionDirectories(opts options) (string, string) {
	homeWasExplicit := opts.home != ""
	home := opts.home
	if home == "" {
		home = os.Getenv("HOME")
	}

	codexDir := opts.codexDir
	if codexDir == "" && !homeWasExplicit {
		if configured := os.Getenv("CODEX_HOME"); configured != "" {
			codexDir = filepath.Join(configured, "sessions")
		}
	}
	if codexDir == "" && home != "" {
		codexDir = filepath.Join(home, ".codex", "sessions")
	}

	claudeDir := opts.claudeDir
	if claudeDir == "" && !homeWasExplicit {
		if configured := os.Getenv("CLAUDE_CONFIG_DIR"); configured != "" {
			claudeDir = filepath.Join(configured, "projects")
		}
	}
	if claudeDir == "" && home != "" {
		claudeDir = filepath.Join(home, ".claude", "projects")
	}
	return codexDir, claudeDir
}

func printSessions(items []session.Session) {
	const rfc3339UTC = "2006-01-02T15:04:05.999999999+00:00"
	for _, item := range items {
		fmt.Printf("%s\t%s\t%s\t%s\t%s\t%s\n",
			item.Agent,
			item.Created.UTC().Format(rfc3339UTC),
			item.Updated.UTC().Format(rfc3339UTC),
			item.ID,
			item.CWD,
			item.Title,
		)
	}
}

func printSkippedWarning(skipped int) {
	if skipped > 0 {
		fmt.Fprintf(os.Stderr, "warning: skipped %d unreadable or unrecognized session file(s)\n", skipped)
	}
}
