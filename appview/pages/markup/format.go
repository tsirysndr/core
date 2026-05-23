package markup

import (
	"regexp"

	"github.com/go-git/go-git/v5/plumbing/filemode"
)

type Format string

const (
	FormatMarkdown Format = "markdown"
	FormatText     Format = "text"
)

var FileTypePatterns = map[Format]*regexp.Regexp{
	FormatMarkdown: regexp.MustCompile(`(?i)\.(md|markdown|mdown|mkdn|mkd)$`),
}

var ReadmePattern = regexp.MustCompile(`(?i)^readme(?:[._-].+)?$`)

// IsReadmeFile reports whether name/mode identifies a readme blob. The git
// mode is checked so directories or symlinks named "readme" are filtered out.
func IsReadmeFile(name, mode string) bool {
	if !ReadmePattern.MatchString(name) {
		return false
	}
	m, err := filemode.New(mode)
	if err != nil {
		return false
	}
	return m == filemode.Regular || m == filemode.Executable
}

// GetFormat returns the Format whose extension list matches filename,
// falling back to FormatText.
func GetFormat(filename string) Format {
	for format, pattern := range FileTypePatterns {
		if pattern.MatchString(filename) {
			return format
		}
	}
	return FormatText
}
