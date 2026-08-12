//go:build !unix

package resume

import (
	"fmt"
	"os"
	"os/exec"
)

// Run starts the selected native agent and waits on platforms without exec.
func Run(spec Spec) error {
	command := exec.Command(spec.Executable, spec.Args...)
	command.Dir = spec.Dir
	command.Stdin = os.Stdin
	command.Stdout = os.Stdout
	command.Stderr = os.Stderr
	if err := command.Run(); err != nil {
		return fmt.Errorf("could not launch %s; make sure it is installed and on PATH: %w", spec.Executable, err)
	}
	return nil
}
