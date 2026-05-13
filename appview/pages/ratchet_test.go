package pages

import (
	"io/fs"
	"regexp"
	"strings"
	"testing"
)

var repoRkeyAllowlist = map[string]bool{
	"templates/repo/settings/sites.html": true,
}

var repoRkeyPattern = regexp.MustCompile(`\.(Repo|RepoInfo)\.Rkey\b`)

func TestNoRepoRkeyInTemplates(t *testing.T) {
	err := fs.WalkDir(Files, "templates", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() || !strings.HasSuffix(path, ".html") {
			return nil
		}
		if repoRkeyAllowlist[path] {
			return nil
		}
		data, err := fs.ReadFile(Files, path)
		if err != nil {
			return err
		}
		for i, line := range strings.Split(string(data), "\n") {
			if repoRkeyPattern.MatchString(line) {
				t.Errorf("%s:%d uses .Repo.Rkey or .RepoInfo.Rkey in URL position. Use .Slug to prefer Name over TID-Rkey.\n  %s",
					path, i+1, strings.TrimSpace(line))
			}
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

var bareDidAllowlist = map[string]bool{
	"templates/strings/string.html":         true,
	"templates/strings/fragments/form.html": true,
	"templates/spindles/dashboard.html":     true,
}

var didCloseAsUrlSegment = regexp.MustCompile(`\.(?:Did|OwnerDid)\s*\}\}\s*/`)
var printfWithUrlFormat = regexp.MustCompile(`printf\s+"[^"]*/[^"]*%s`)
var didArgRef = regexp.MustCompile(`\b[\$\.]\w+(?:\.\w+)*\.(?:Did|OwnerDid)\b`)

func TestNoBareDidInTemplateRepoUrls(t *testing.T) {
	err := fs.WalkDir(Files, "templates", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() || !strings.HasSuffix(path, ".html") {
			return nil
		}
		if bareDidAllowlist[path] {
			return nil
		}
		data, err := fs.ReadFile(Files, path)
		if err != nil {
			return err
		}
		for i, line := range strings.Split(string(data), "\n") {
			for _, idx := range didCloseAsUrlSegment.FindAllStringIndex(line, -1) {
				openIdx := strings.LastIndex(line[:idx[0]], "{{")
				if openIdx == -1 {
					continue
				}
				action := line[openIdx:idx[1]]
				if !strings.Contains(action, "resolve") {
					t.Errorf("%s:%d renders raw DID as URL path segment. Wrap in `resolve` so the handle appears.\n  %s",
						path, i+1, strings.TrimSpace(line))
					break
				}
			}
			if printfWithUrlFormat.MatchString(line) && didArgRef.MatchString(line) && !strings.Contains(line, "resolve ") {
				t.Errorf("%s:%d builds a URL path via printf with a raw DID arg. Wrap the DID in `resolve` so handle appears.\n  %s",
					path, i+1, strings.TrimSpace(line))
			}
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}
