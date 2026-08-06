package types

import (
	"github.com/bluekeyes/go-gitdiff/gitdiff"
	"github.com/go-git/go-git/v5/plumbing/object"
)

type RepoIndexResponse struct {
	IsEmpty        bool
	Ref            string
	Readme         string
	ReadmeFileName string
	Commits        []Commit
	Files          []NiceTree
	Branches       []Branch
	TotalBranches  int
	Tags           []*TagReference
	TotalTags      int
	TotalCommits   int
}

type RepoLogResponse struct {
	Commits []Commit `json:"commits,omitempty"`
	Ref     string   `json:"ref,omitempty"`
	Total   int      `json:"total,omitempty"`
	Page    int      `json:"page,omitempty"`
}

type RepoCommitResponse struct {
	Ref  string    `json:"ref,omitempty"`
	Diff *NiceDiff `json:"diff,omitempty"`
}

type RepoFormatPatchResponse struct {
	Rev1             string          `json:"rev1,omitempty"`
	Rev2             string          `json:"rev2,omitempty"`
	FormatPatch      []FormatPatch   `json:"format_patch,omitempty"`
	FormatPatchRaw   string          `json:"patch,omitempty"`
	CombinedPatch    []*gitdiff.File `json:"combined_patch,omitempty"`
	CombinedPatchRaw string          `json:"combined_patch_raw,omitempty"`
}

type TagReference struct {
	Reference
	Tag     *object.Tag `json:"tag,omitempty"`
	Message string      `json:"message,omitempty"`
}

type Reference struct {
	Name string `json:"name"`
	Hash string `json:"hash"`
}

type Branch struct {
	Reference `json:"reference"`
	Commit    *object.Commit `json:"commit,omitempty"`
	IsDefault bool           `json:"is_default,omitempty"`
}

type RepoTagsResponse struct {
	Tags  []*TagReference `json:"tags,omitempty"`
	Total int             `json:"total,omitempty"`
}

type RepoTagResponse struct {
	Tag *TagReference `json:"tag,omitempty"`
}

type RepoBranchesResponse struct {
	Branches []Branch `json:"branches,omitempty"`
	Total    int      `json:"total,omitempty"`
}

type RepoBranchResponse struct {
	Branch Branch
}

type RepoDefaultBranchResponse struct {
	Branch string `json:"branch,omitempty"`
}

type ForkStatus int

const (
	UpToDate        ForkStatus = 0
	FastForwardable ForkStatus = 1
	Conflict        ForkStatus = 2
	MissingBranch   ForkStatus = 3
)

type ForkInfo struct {
	IsFork bool
	Status ForkStatus
}

type RepoLanguageDetails struct {
	Name       string
	Percentage float32
	Color      string
}

type RepoLanguageResponse struct {
	// Language: File count
	Languages map[string]int64 `json:"languages"`
}
