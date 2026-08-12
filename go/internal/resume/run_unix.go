//go:build unix

package resume

import (
	"fmt"
	"os"
	"os/exec"
	"syscall"
)

// Run replaces agent-resume-go with the selected native agent on Unix.
func Run(spec Spec) error {
	path, err := exec.LookPath(spec.Executable)
	if err != nil {
		return fmt.Errorf("could not launch %s; make sure it is installed and on PATH: %w", spec.Executable, err)
	}
	if spec.Dir != "" {
		if err := os.Chdir(spec.Dir); err != nil {
			return fmt.Errorf("could not change directory to %s: %w", spec.Dir, err)
		}
	}
	argv := append([]string{spec.Executable}, spec.Args...)
	return syscall.Exec(path, argv, os.Environ())
}
