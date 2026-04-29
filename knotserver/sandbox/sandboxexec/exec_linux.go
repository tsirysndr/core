//go:build linux

package sandboxexec

import (
	"tangled.org/core/knotserver/sandbox"
)

func applyAndExec(repoPaths, gitArgs []string) error {
	return sandbox.ApplyLandlock(repoPaths, gitArgs)
}
