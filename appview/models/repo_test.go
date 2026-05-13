package models

import (
	"strings"
	"testing"
)

func TestValidateRepoName_ValidRkeys(t *testing.T) {
	valid := []string{
		"myrepo",
		"MyRepo",
		"my-repo",
		"my_repo",
		"my.repo",
		"a",
		"repo123",
		strings.Repeat("a", 100),
	}
	for _, name := range valid {
		if err := ValidateRepoName(name); err != nil {
			t.Errorf("ValidateRepoName(%q) = %v, want nil", name, err)
		}
	}
}

func TestValidateRepoName_InvalidRkeys(t *testing.T) {
	cases := []struct {
		input  string
		substr string
	}{
		{"", "empty"},
		{strings.Repeat("a", 101), "100 characters"},
		{"has space", "alphanumeric"},
		{"has/slash", "invalid path"},
		{"has\\backslash", "invalid path"},
		{".dotprefix", "invalid path"},
		{"dotsuffix.", "invalid path"},
		{"two..dots", "sequential dots"},
		{"../traversal", "invalid path"},
		{"self", "reserved"},
		{"SELF", "reserved"},
	}
	for _, tc := range cases {
		err := ValidateRepoName(tc.input)
		if err == nil {
			t.Errorf("ValidateRepoName(%q) = nil, want error containing %q", tc.input, tc.substr)
			continue
		}
		if !strings.Contains(strings.ToLower(err.Error()), strings.ToLower(tc.substr)) {
			t.Errorf("ValidateRepoName(%q) = %q, want substring %q", tc.input, err.Error(), tc.substr)
		}
	}
}

func TestStripGitExt(t *testing.T) {
	cases := []struct{ in, want string }{
		{"repo.git", "repo"},
		{"repo", "repo"},
		{"repo.git.git", "repo.git"},
		{".git", ""},
	}
	for _, tc := range cases {
		if got := StripGitExt(tc.in); got != tc.want {
			t.Errorf("StripGitExt(%q) = %q, want %q", tc.in, got, tc.want)
		}
	}
}

func TestCosmeticName_NilWhenMatchesRkey(t *testing.T) {
	r := Repo{Name: "myrepo", Rkey: "myrepo"}
	rec := r.AsRecord()
	if rec.Name != nil {
		t.Errorf("cosmeticName should be nil when Name == Rkey, got %q", *rec.Name)
	}
}

func TestCosmeticName_PresentWhenDiffers(t *testing.T) {
	r := Repo{Name: "MyRepo", Rkey: "myrepo", Knot: "k"}
	rec := r.AsRecord()
	if rec.Name == nil {
		t.Fatal("cosmeticName should be non-nil when Name != Rkey")
	}
	if *rec.Name != "MyRepo" {
		t.Errorf("cosmeticName = %q, want %q", *rec.Name, "MyRepo")
	}
}

func TestRepoSlug(t *testing.T) {
	cases := []struct {
		name string
		repo Repo
		want string
	}{
		{"name set distinct from rkey", Repo{Name: "anemone", Rkey: "3kabc"}, "anemone"},
		{"name equals rkey", Repo{Name: "scallop", Rkey: "scallop"}, "scallop"},
		{"name empty falls to rkey", Repo{Name: "", Rkey: "whelk"}, "whelk"},
		{"both empty", Repo{}, ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := c.repo.Slug(); got != c.want {
				t.Errorf("Slug() = %q, want %q", got, c.want)
			}
		})
	}
}
