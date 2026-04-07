package state

import "testing"

func TestParseSortParam(t *testing.T) {
	tests := []struct {
		input     string
		wantField string
		wantDesc  bool
	}{
		{"", "relevance", true},
		{"stars-desc", "stars", true},
		{"stars-asc", "stars", false},
		{"created-desc", "created", true},
		{"created-asc", "created", false},
		{"relevance-asc", "relevance", false},
		{"issues-desc", "issues", true},
		{"pulls-asc", "pulls", false},
		// invalid field → default
		{"forks-desc", "relevance", true},
		{"unknown-asc", "relevance", true},
		// malformed → default
		{"nodash", "relevance", true},
		{"too-many-parts-here", "relevance", true},
		{"-", "relevance", true},
	}

	for _, tt := range tests {
		field, desc := parseSortParam(tt.input)
		if field != tt.wantField || desc != tt.wantDesc {
			t.Errorf("parseSortParam(%q) = (%q, %v), want (%q, %v)",
				tt.input, field, desc, tt.wantField, tt.wantDesc)
		}
	}
}
