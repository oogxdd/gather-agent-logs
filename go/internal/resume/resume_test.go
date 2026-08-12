package resume

import (
	"path/filepath"
	"reflect"
	"testing"

	"github.com/oogxdd/gather-agent-logs/go/internal/session"
)

func TestCodexResumeSpec(t *testing.T) {
	dir := t.TempDir()
	spec, err := For(session.Session{
		Agent: session.Codex,
		ID:    "019ff21e-4824-70d2-8cb6-57e5b1aebefb",
		CWD:   dir,
	})
	if err != nil {
		t.Fatal(err)
	}
	if spec.Executable != "codex" || !reflect.DeepEqual(spec.Args, []string{"resume", "019ff21e-4824-70d2-8cb6-57e5b1aebefb"}) || spec.Dir != dir {
		t.Fatalf("unexpected spec: %+v", spec)
	}
}

func TestClaudeResumeSpec(t *testing.T) {
	dir := t.TempDir()
	spec, err := For(session.Session{
		Agent: session.Claude,
		ID:    "86e1a7f3-2e99-483f-b743-64e0003c58c8",
		CWD:   dir,
	})
	if err != nil {
		t.Fatal(err)
	}
	if spec.Executable != "claude" || !reflect.DeepEqual(spec.Args, []string{"--resume", "86e1a7f3-2e99-483f-b743-64e0003c58c8"}) {
		t.Fatalf("unexpected spec: %+v", spec)
	}
}

func TestResumeRejectsMissingDirectory(t *testing.T) {
	_, err := For(session.Session{Agent: session.Codex, ID: "session-id", CWD: filepath.Join(t.TempDir(), "missing")})
	if err == nil {
		t.Fatal("expected missing-directory error")
	}
}
