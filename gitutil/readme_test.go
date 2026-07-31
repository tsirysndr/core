package gitutil

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
		{"ReadMe.MD", fileMode, true},
		{"readme", fileMode, true},
		{"README", execMode, true},

		// regression: a directory named "readme" must not be picked up
		// as the README blob; tree handlers used to fetch it and 503.
		{"readme", dirMode, false},
		{"README.md", dirMode, false},

		// readme is matched by convention, not by renderable format;
		// unsupported markup falls through to plaintext in GetFormat.
		{"README.rst", fileMode, true},
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
