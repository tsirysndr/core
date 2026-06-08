package markup

import "testing"

func TestIsReadmeFile(t *testing.T) {
	const (
		fileMode = "100644"
		execMode = "100755"
		dirMode  = "040000"
	)

	cases := []struct {
		name string
		mode string
		want bool
	}{
		{"README.md", fileMode, true},
		{"readme.md", fileMode, true},
		{"ReadMe.MD", fileMode, true},
		{"README.markdown", fileMode, true},
		{"README.mdown", fileMode, true},
		{"README.mkdn", fileMode, true},
		{"README.mkd", fileMode, true},
		{"README.txt", fileMode, true},
		{"readme", fileMode, true},
		{"README", execMode, true},

		// regression: a directory named "readme" must not be picked up
		// as the README blob; tree handlers used to fetch it and 503.
		{"readme", dirMode, false},
		{"README.md", dirMode, false},

		// readme is matched by convention, not by renderable format;
		// unsupported markup falls through to plaintext in GetFormat.
		{"README.rst", fileMode, true},
		{"README.org", fileMode, true},
		{"README.asciidoc", fileMode, true},
		{"README.foo", fileMode, true},

		{"README.Music.md", fileMode, false},
		{"readme-old", fileMode, false},
		{"readme_legacy", fileMode, false},
		{"notreadme.md", fileMode, false},
		{"READMEISH", fileMode, false},
		{"README.md", "", false},
		{"README.md", "120000", false}, // symlink
	}

	for _, c := range cases {
		t.Run(c.name+"/"+c.mode, func(t *testing.T) {
			if got := IsReadmeFile(c.name, c.mode); got != c.want {
				t.Errorf("IsReadmeFile(%q, %q) = %v, want %v", c.name, c.mode, got, c.want)
			}
		})
	}
}

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
