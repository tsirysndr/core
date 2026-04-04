package models

import (
	"testing"
)

func TestIssueSearchOptions_HasSearchFilters(t *testing.T) {
	tests := []struct {
		name string
		opts IssueSearchOptions
		want bool
	}{
		{
			name: "zero value returns false",
			opts: IssueSearchOptions{},
			want: false,
		},
		{
			name: "non-filter fields only (RepoAt, IsOpen, Page) return false",
			opts: IssueSearchOptions{RepoAt: "at://did:plc:abc/repo"},
			want: false,
		},
		{
			name: "keyword set",
			opts: IssueSearchOptions{Keywords: []string{"bug"}},
			want: true,
		},
		{
			name: "phrase set",
			opts: IssueSearchOptions{Phrases: []string{"null pointer"}},
			want: true,
		},
		{
			name: "author did set",
			opts: IssueSearchOptions{AuthorDid: "did:plc:abc"},
			want: true,
		},
		{
			name: "label set",
			opts: IssueSearchOptions{Labels: []string{"bug"}},
			want: true,
		},
		{
			name: "label value set",
			opts: IssueSearchOptions{LabelValues: []string{"priority:high"}},
			want: true,
		},
		{
			name: "negated keyword set",
			opts: IssueSearchOptions{NegatedKeywords: []string{"wontfix"}},
			want: true,
		},
		{
			name: "negated phrase set",
			opts: IssueSearchOptions{NegatedPhrases: []string{"not a bug"}},
			want: true,
		},
		{
			name: "negated label set",
			opts: IssueSearchOptions{NegatedLabels: []string{"duplicate"}},
			want: true,
		},
		{
			name: "negated label value set",
			opts: IssueSearchOptions{NegatedLabelValues: []string{"priority:low"}},
			want: true,
		},
		{
			name: "negated author did set",
			opts: IssueSearchOptions{NegatedAuthorDids: []string{"did:plc:xyz"}},
			want: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.opts.HasSearchFilters(); got != tt.want {
				t.Errorf("HasSearchFilters() = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestPullSearchOptions_HasSearchFilters(t *testing.T) {
	tests := []struct {
		name string
		opts PullSearchOptions
		want bool
	}{
		{
			name: "zero value returns false",
			opts: PullSearchOptions{},
			want: false,
		},
		{
			name: "non-filter fields only (RepoAt, State, Page) return false",
			opts: PullSearchOptions{RepoAt: "at://did:plc:abc/repo"},
			want: false,
		},
		{
			name: "keyword set",
			opts: PullSearchOptions{Keywords: []string{"refactor"}},
			want: true,
		},
		{
			name: "phrase set",
			opts: PullSearchOptions{Phrases: []string{"breaking change"}},
			want: true,
		},
		{
			name: "author did set",
			opts: PullSearchOptions{AuthorDid: "did:plc:abc"},
			want: true,
		},
		{
			name: "label set",
			opts: PullSearchOptions{Labels: []string{"enhancement"}},
			want: true,
		},
		{
			name: "label value set",
			opts: PullSearchOptions{LabelValues: []string{"size:large"}},
			want: true,
		},
		{
			name: "negated keyword set",
			opts: PullSearchOptions{NegatedKeywords: []string{"wip"}},
			want: true,
		},
		{
			name: "negated phrase set",
			opts: PullSearchOptions{NegatedPhrases: []string{"do not merge"}},
			want: true,
		},
		{
			name: "negated label set",
			opts: PullSearchOptions{NegatedLabels: []string{"blocked"}},
			want: true,
		},
		{
			name: "negated label value set",
			opts: PullSearchOptions{NegatedLabelValues: []string{"size:small"}},
			want: true,
		},
		{
			name: "negated author did set",
			opts: PullSearchOptions{NegatedAuthorDids: []string{"did:plc:xyz"}},
			want: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.opts.HasSearchFilters(); got != tt.want {
				t.Errorf("HasSearchFilters() = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestRepoSearchOptions_HasSearchFilters(t *testing.T) {
	tests := []struct {
		name string
		opts RepoSearchOptions
		want bool
	}{
		{
			name: "zero value returns false",
			opts: RepoSearchOptions{},
			want: false,
		},
		{
			name: "non-filter fields only (Knot, Did, Page) return false",
			opts: RepoSearchOptions{
				Knot: "knot.example.com",
				Did:  "did:plc:abc",
			},
			want: false,
		},
		{
			name: "keyword set",
			opts: RepoSearchOptions{Keywords: []string{"parser"}},
			want: true,
		},
		{
			name: "phrase set",
			opts: RepoSearchOptions{Phrases: []string{"http client"}},
			want: true,
		},
		{
			name: "language set",
			opts: RepoSearchOptions{Language: "Go"},
			want: true,
		},
		{
			name: "topic set",
			opts: RepoSearchOptions{Topics: []string{"networking"}},
			want: true,
		},
		{
			name: "negated keyword set",
			opts: RepoSearchOptions{NegatedKeywords: []string{"deprecated"}},
			want: true,
		},
		{
			name: "negated phrase set",
			opts: RepoSearchOptions{NegatedPhrases: []string{"work in progress"}},
			want: true,
		},
		{
			name: "negated topic set",
			opts: RepoSearchOptions{NegatedTopics: []string{"archived"}},
			want: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.opts.HasSearchFilters(); got != tt.want {
				t.Errorf("HasSearchFilters() = %v, want %v", got, tt.want)
			}
		})
	}
}
