package gitutil

import (
	"regexp"

	"github.com/go-git/go-git/v5/plumbing/filemode"
)

var readmePattern = regexp.MustCompile(`(?i)^readme(?:\.[^.]+)?$`)

// IsReadmeFile reports whether name/mode identifies a readme blob. The git
// mode is checked so directories or symlinks named "readme" are filtered out.
func IsReadmeFile(name, mode string) bool {
	if !readmePattern.MatchString(name) {
		return false
	}
	m, err := filemode.New(mode)
	if err != nil {
		return false
	}
	return m == filemode.Regular || m == filemode.Executable
}
