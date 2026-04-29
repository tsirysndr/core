//go:build !linux

package sandboxexec

import "fmt"

func applyAndExec(repoPaths, gitArgs []string) error {
	return fmt.Errorf("sandbox-exec is only supported on Linux")
}
