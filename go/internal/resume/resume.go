package resume

import (
	"fmt"
	"os"

	"github.com/oogxdd/gather-agent-logs/go/internal/session"
)

// Spec describes the native command required to resume a session.
type Spec struct {
	Executable string
	Args       []string
	Dir        string
}

// For builds a native Codex or Claude Code resume command.
func For(item session.Session) (Spec, error) {
	if item.ID == "" {
		return Spec{}, fmt.Errorf("session has no ID")
	}
	if item.CWD != "" {
		info, err := os.Stat(item.CWD)
		if err != nil || !info.IsDir() {
			return Spec{}, fmt.Errorf("saved working directory no longer exists: %s", item.CWD)
		}
	}

	spec := Spec{Executable: item.Agent.Executable(), Dir: item.CWD}
	if item.Agent == session.Codex {
		spec.Args = []string{"resume", item.ID}
	} else {
		spec.Args = []string{"--resume", item.ID}
	}
	return spec, nil
}
