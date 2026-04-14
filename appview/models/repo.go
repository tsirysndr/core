package models

import (
	"fmt"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	securejoin "github.com/cyphar/filepath-securejoin"
	"tangled.org/core/api/tangled"
)

type Repo struct {
	Id          int64
	Did         string
	Name        string
	Knot        string
	Rkey        string
	Created     time.Time
	Description string
	Website     string
	Topics      []string
	Spindle     string
	Labels      []string
	RepoDid     string

	// optionally, populate this when querying for reverse mappings
	RepoStats *RepoStats

	// optional
	Source string
}

func (r *Repo) AsRecord() tangled.Repo {
	var source, spindle, description, website *string

	if r.Source != "" {
		source = &r.Source
	}

	if r.Spindle != "" {
		spindle = &r.Spindle
	}

	if r.Description != "" {
		description = &r.Description
	}

	if r.Website != "" {
		website = &r.Website
	}

	var repoDid *string
	if r.RepoDid != "" {
		repoDid = &r.RepoDid
	}

	return tangled.Repo{
		Knot:        r.Knot,
		Name:        r.cosmeticName(),
		Description: description,
		Website:     website,
		Topics:      r.Topics,
		CreatedAt:   r.Created.Format(time.RFC3339),
		Source:      source,
		Spindle:     spindle,
		Labels:      r.Labels,
		RepoDid:     repoDid,
	}
}

func (r *Repo) cosmeticName() *string {
	if r.Name == "" || r.Name == r.Rkey {
		return nil
	}
	return &r.Name
}

func (r Repo) RepoAt() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", r.Did, tangled.RepoNSID, r.Rkey))
}

func (r Repo) RepoIdentifier() string {
	if r.RepoDid != "" {
		return r.RepoDid
	}
	p, _ := securejoin.SecureJoin(r.Did, r.Rkey)
	return p
}

func (r Repo) PinIdentifier() string {
	if r.RepoDid != "" {
		return r.RepoDid
	}
	return string(r.RepoAt())
}

func (r Repo) TopicStr() string {
	return strings.Join(r.Topics, " ")
}

type RepoStats struct {
	Language   string
	StarCount  int
	IssueCount IssueCount
	PullCount  PullCount
}

type IssueCount struct {
	Open   int
	Closed int
}

type PullCount struct {
	Open    int
	Merged  int
	Closed  int
	Deleted int
}

type RepoLabel struct {
	Id      int64
	RepoDid syntax.DID
	LabelAt syntax.ATURI
}

var reservedRepoNames = map[string]struct{}{
	"self": {},
}

func ValidateRepoName(name string) error {
	if len(name) == 0 {
		return fmt.Errorf("Repository name cannot be empty")
	}
	if len(name) > 100 {
		return fmt.Errorf("Repository name must be 100 characters or fewer")
	}

	// check for path traversal attempts
	if strings.Contains(name, "/") || strings.Contains(name, "\\") {
		return fmt.Errorf("Repository name contains invalid path characters")
	}

	// check for sequences that could be used for traversal when normalized
	if strings.HasPrefix(name, ".") || strings.HasSuffix(name, ".") {
		return fmt.Errorf("Repository name contains invalid path sequence")
	}

	// then continue with character validation
	for _, char := range name {
		if !((char >= 'a' && char <= 'z') ||
			(char >= 'A' && char <= 'Z') ||
			(char >= '0' && char <= '9') ||
			char == '-' || char == '_' || char == '.') {
			return fmt.Errorf("Repository name can only contain alphanumeric characters, periods, hyphens, and underscores")
		}
	}

	// additional check to prevent multiple sequential dots
	if strings.Contains(name, "..") {
		return fmt.Errorf("Repository name cannot contain sequential dots")
	}

	if _, reserved := reservedRepoNames[strings.ToLower(name)]; reserved {
		return fmt.Errorf("Repository name %q is reserved", name)
	}

	// if all checks pass
	return nil
}

func StripGitExt(name string) string {
	return strings.TrimSuffix(name, ".git")
}

type RepoGroup struct {
	Repo   *Repo
	Issues []Issue
}

type BlobContentType int

const (
	BlobContentTypeCode BlobContentType = iota
	BlobContentTypeMarkup
	BlobContentTypeImage
	BlobContentTypeSvg
	BlobContentTypeVideo
	BlobContentTypeSubmodule
)

func (ty BlobContentType) IsCode() bool      { return ty == BlobContentTypeCode }
func (ty BlobContentType) IsMarkup() bool    { return ty == BlobContentTypeMarkup }
func (ty BlobContentType) IsImage() bool     { return ty == BlobContentTypeImage }
func (ty BlobContentType) IsSvg() bool       { return ty == BlobContentTypeSvg }
func (ty BlobContentType) IsVideo() bool     { return ty == BlobContentTypeVideo }
func (ty BlobContentType) IsSubmodule() bool { return ty == BlobContentTypeSubmodule }

type BlobView struct {
	HasTextView     bool // can show as code/text
	HasRenderedView bool // can show rendered (markup/image/video/submodule)
	HasRawView      bool // can download raw (everything except submodule)
	FileTooLarge    bool // file too large (ignored for image files)

	// current display mode
	ShowingRendered bool // currently in rendered mode

	// content type flags
	ContentType BlobContentType

	// Content data
	Contents   string
	ContentSrc string // URL for media files
	Lines      int
	SizeHint   uint64
}

// if both views are available, then show a toggle between them
func (b BlobView) ShowToggle() bool {
	return b.HasTextView && b.HasRenderedView
}

func (b BlobView) IsUnsupported() bool {
	// no view available, only raw
	return !(b.HasRenderedView || b.HasTextView)
}

func (b BlobView) ShowingText() bool {
	return !b.ShowingRendered
}
