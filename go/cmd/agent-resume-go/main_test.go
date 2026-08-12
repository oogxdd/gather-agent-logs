package main

import (
	"path/filepath"
	"testing"
)

func TestAgentSelection(t *testing.T) {
	codex, claude, err := agentSelection("claude")
	if err != nil || codex || !claude {
		t.Fatalf("unexpected selection: codex=%v claude=%v err=%v", codex, claude, err)
	}
	if _, _, err := agentSelection("other"); err == nil {
		t.Fatal("expected invalid-agent error")
	}
}

func TestExplicitHomeIgnoresAgentEnvironment(t *testing.T) {
	t.Setenv("CODEX_HOME", "/configured/codex")
	t.Setenv("CLAUDE_CONFIG_DIR", "/configured/claude")
	codex, claude := sessionDirectories(options{home: "/alternate/home"})

	if codex != filepath.Join("/alternate/home", ".codex", "sessions") {
		t.Fatalf("codex directory = %q", codex)
	}
	if claude != filepath.Join("/alternate/home", ".claude", "projects") {
		t.Fatalf("claude directory = %q", claude)
	}
}

func TestAgentEnvironmentIsUsedByDefault(t *testing.T) {
	t.Setenv("HOME", "/default/home")
	t.Setenv("CODEX_HOME", "/configured/codex")
	t.Setenv("CLAUDE_CONFIG_DIR", "/configured/claude")
	codex, claude := sessionDirectories(options{})

	if codex != filepath.Join("/configured/codex", "sessions") {
		t.Fatalf("codex directory = %q", codex)
	}
	if claude != filepath.Join("/configured/claude", "projects") {
		t.Fatalf("claude directory = %q", claude)
	}
}
