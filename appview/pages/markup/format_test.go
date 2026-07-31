package markup

import "testing"

func TestFileTypePatterns(t *testing.T) {
	cases := []struct {
		format   Format
		filename string
		want     bool
	}{
		{FormatMarkdown, "x.md", true},
		{FormatMarkdown, "x.MARKDOWN", true},
		{FormatMarkdown, "x.mkdn", true},
		{FormatMarkdown, "x.mkd", true},
		{FormatMarkdown, "x.mdown", true},
		{FormatMarkdown, "x.txt", false},
		{FormatMarkdown, "x.rst", false},
	}

	for _, c := range cases {
		t.Run(string(c.format)+"/"+c.filename, func(t *testing.T) {
			p, ok := FileTypePatterns[c.format]
			if !ok {
				t.Fatalf("FileTypePatterns[%q] missing", c.format)
			}
			if got := p.MatchString(c.filename); got != c.want {
				t.Errorf("FileTypePatterns[%q].MatchString(%q) = %v, want %v", c.format, c.filename, got, c.want)
			}
		})
	}
}

func TestGetFormat(t *testing.T) {
	cases := []struct {
		filename string
		want     Format
	}{
		{"x.md", FormatMarkdown},
		{"x.MARKDOWN", FormatMarkdown},
		{"x.txt", FormatText},
		{"x.rs", FormatText}, // unknown -> default
		{"noext", FormatText},
	}

	for _, c := range cases {
		t.Run(c.filename, func(t *testing.T) {
			if got := GetFormat(c.filename); got != c.want {
				t.Errorf("GetFormat(%q) = %q, want %q", c.filename, got, c.want)
			}
		})
	}
}
