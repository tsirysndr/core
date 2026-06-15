package models

import (
	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sourcegraph/zoekt"
)

// Result is a single matched file. A zoekt FileMatch is either a filename
// match or a set of content matches, never both: File is set for the former,
// Chunks for the latter.
type Result struct {
	RepoDID  syntax.DID
	FilePath string
	Branches []string // branch names
	Commit   string   // commit id
	Language string

	File   *Result_FileMatch   // set for a filename match
	Chunks []Result_ChunkMatch // set for content matches
}

type Result_FileMatch struct {
	// Ranges are the matched span(s) within FilePath. LineNumber is always 1 for
	// filename matches; Column is 1-based and in runes.
	Ranges []zoekt.Range
}

type Result_ChunkMatch struct {
	// Content is a contiguous run of complete lines that fully contains Ranges.
	Content string
	// ContentStartLine is the 1-based line number of Content's first line.
	ContentStartLine int
	// Ranges are the matched span(s) within the file. LineNumber/Column are
	// 1-based, Column is in runes. A Range may span multiple lines.
	Ranges []zoekt.Range
}

// IsFileMatch tells if search result is from file-name match
func (r *Result) IsFileMatch() bool {
	return r.File != nil
}

// IsChunkMatch tells if search result is from chunk match
func (r *Result) IsChunkMatch() bool {
	return len(r.Chunks) > 0
}
