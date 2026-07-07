package models

import (
	"fmt"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	securejoin "github.com/cyphar/filepath-securejoin"
	enry "github.com/go-enry/go-enry/v2"
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
		RepoDid:     r.RepoDidPtr(),
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

func (r Repo) Slug() string {
	if r.Name != "" {
		return r.Name
	}
	return r.Rkey
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

func (r Repo) RepoDidPtr() *string {
	if r.RepoDid == "" {
		return nil
	}
	return &r.RepoDid
}

func (r Repo) TopicStr() string {
	return strings.Join(r.Topics, " ")
}

type RepoStats struct {
	Language   string
	StarCount  int
	IssueCount IssueCount
	PullCount  PullCount
	ForkCount  int
}

// returns the first file extension for the language ("ts" for typescript) as
// an uppercase string
func (s *RepoStats) LangShortName() string {
	if s == nil || s.Language == "" {
		return ""
	}
	exts := enry.GetLanguageExtensions(s.Language)
	if len(exts) > 0 {
		// extensions include the leading dot, e.g. ".ts" -> "TS"
		return strings.ToUpper(strings.TrimPrefix(exts[0], "."))
	}
	return s.Language
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
	BlobContentTypeOther
)

func (ty BlobContentType) IsCode() bool      { return ty == BlobContentTypeCode }
func (ty BlobContentType) IsMarkup() bool    { return ty == BlobContentTypeMarkup }
func (ty BlobContentType) IsImage() bool     { return ty == BlobContentTypeImage }
func (ty BlobContentType) IsSvg() bool       { return ty == BlobContentTypeSvg }
func (ty BlobContentType) IsVideo() bool     { return ty == BlobContentTypeVideo }
func (ty BlobContentType) IsSubmodule() bool { return ty == BlobContentTypeSubmodule }
func (ty BlobContentType) HasTextView() bool {
	return ty == BlobContentTypeCode || ty == BlobContentTypeMarkup || ty == BlobContentTypeSvg
}
func (ty BlobContentType) HasRenderedView() bool {
	return ty != BlobContentTypeCode && ty != BlobContentTypeOther
}
func (ty BlobContentType) HasRawView() bool {
	return ty != BlobContentTypeSubmodule
}

type BlobView struct {
	// content type flags
	ContentType BlobContentType

	// Content data
	ContentSrc   string // URL to raw content
	Contents     string // textual content
	FileTooLarge bool   // textual content is too large
	Lines        int    // line count of textual content
	SizeHint     uint64
}
