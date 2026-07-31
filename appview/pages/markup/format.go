package markup

import (
	"regexp"
)

type Format string

const (
	FormatMarkdown Format = "markdown"
	FormatText     Format = "text"
)

var FileTypePatterns = map[Format]*regexp.Regexp{
	FormatMarkdown: regexp.MustCompile(`(?i)\.(md|markdown|mdown|mkdn|mkd)$`),
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
