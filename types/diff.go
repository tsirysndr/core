package types

import (
	"net/url"

	"github.com/bluekeyes/go-gitdiff/gitdiff"
	"tangled.org/core/appview/filetree"
)

type DiffOpts struct {
	Split bool `json:"split"`
}

func (d DiffOpts) Encode() string {
	values := make(url.Values)
	if d.Split {
		values.Set("diff", "split")
	} else {
		values.Set("diff", "unified")
	}
	return values.Encode()
}

// A nicer git diff representation.
type NiceDiff struct {
	Commit Commit   `json:"commit"`
	Stat   DiffStat `json:"stat"`
	Diff   []Diff   `json:"diff"`
}

type Diff struct {
	Name struct {
		Old string `json:"old"`
		New string `json:"new"`
	} `json:"name"`
	TextFragments []gitdiff.TextFragment `json:"text_fragments"`
	IsBinary      bool                   `json:"is_binary"`
	IsNew         bool                   `json:"is_new"`
	IsDelete      bool                   `json:"is_delete"`
	IsCopy        bool                   `json:"is_copy"`
	IsRename      bool                   `json:"is_rename"`
}

func (d Diff) Stats() DiffFileStat {
	var stats DiffFileStat
	for _, f := range d.TextFragments {
		stats.Insertions += f.LinesAdded
		stats.Deletions += f.LinesDeleted
	}
	return stats
}

type DiffStat struct {
	Insertions   int64 `json:"insertions"`
	Deletions    int64 `json:"deletions"`
	FilesChanged int   `json:"files_changed"`
}

type DiffFileStat struct {
	Insertions int64
	Deletions  int64
}

type DiffTree struct {
	Rev1  string          `json:"rev1"`
	Rev2  string          `json:"rev2"`
	Patch string          `json:"patch"`
	Diff  []*gitdiff.File `json:"diff"`
}

type DiffFileName struct {
	Old string
	New string
}

func (d NiceDiff) ChangedFiles() []DiffFileRenderer {
	drs := make([]DiffFileRenderer, len(d.Diff))
	for i, s := range d.Diff {
		drs[i] = s
	}
	return drs
}

func (d NiceDiff) FileTree() *filetree.FileTreeNode {
	fs := make([]string, len(d.Diff))
	for i, s := range d.Diff {
		fs[i] = s.Id()
	}
	return filetree.FileTree(fs)
}

func (d NiceDiff) Stats() DiffStat {
	return d.Stat
}

func (d Diff) Id() string {
	if d.IsDelete {
		return d.Name.Old
	}
	return d.Name.New
}

func (d Diff) Names() DiffFileName {
	var n DiffFileName
	if d.IsDelete {
		n.Old = d.Name.Old
		return n
	} else if d.IsCopy || d.IsRename {
		n.Old = d.Name.Old
		n.New = d.Name.New
		return n
	} else {
		n.New = d.Name.New
		return n
	}
}

func (d Diff) CanRender() string {
	if d.IsBinary {
		return "This is a binary file and will not be displayed."
	}

	return ""
}

func (d Diff) Split() SplitDiff {
	fragments := make([]SplitFragment, len(d.TextFragments))
	for i, fragment := range d.TextFragments {
		leftLines, rightLines := SeparateLines(&fragment)
		fragments[i] = SplitFragment{
			Header:     fragment.Header(),
			LeftLines:  leftLines,
			RightLines: rightLines,
		}
	}

	return SplitDiff{
		Name:          d.Id(),
		TextFragments: fragments,
	}
}
