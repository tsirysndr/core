package types

import (
	"bytes"
	"encoding/json"
	"fmt"
	"maps"
	"regexp"
	"strings"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
)

type Commit struct {
	// hash of the commit object.
	Hash plumbing.Hash `json:"hash,omitempty"`

	// author is the original author of the commit.
	Author object.Signature `json:"author"`

	// committer is the one performing the commit, might be different from author.
	Committer object.Signature `json:"committer"`

	// message is the commit message, contains arbitrary text.
	Message string `json:"message"`

	// treehash is the hash of the root tree of the commit.
	Tree string `json:"tree"`

	// parents are the hashes of the parent commits of the commit.
	ParentHashes []plumbing.Hash `json:"parent_hashes,omitempty"`

	// pgpsignature is the pgp signature of the commit.
	PGPSignature string `json:"pgp_signature,omitempty"`

	// mergetag is the embedded tag object when a merge commit is created by
	// merging a signed tag.
	MergeTag string `json:"merge_tag,omitempty"`

	// changeid is a unique identifier for the change (e.g., gerrit change-id).
	ChangeId string `json:"change_id,omitempty"`

	// extraheaders contains additional headers not captured by other fields.
	ExtraHeaders map[string][]byte `json:"extra_headers,omitempty"`

	// deprecated: kept for backwards compatibility with old json format.
	This string `json:"this,omitempty"`

	// deprecated: kept for backwards compatibility with old json format.
	Parent string `json:"parent,omitempty"`
}

// types.Commit is an unify two commit structs:
//   - git.object.Commit from
//   - types.NiceDiff.commit
//
// to do this in backwards compatible fashion, we define the base struct
// to use the same fields as NiceDiff.Commit, and then we also unmarshal
// the struct fields from go-git structs, this custom unmarshal makes sense
// of both representations and unifies them to have maximal data in either
// form.
func (c *Commit) UnmarshalJSON(data []byte) error {
	type Alias Commit

	aux := &struct {
		*object.Commit
		*Alias
	}{
		Alias: (*Alias)(c),
	}

	if err := json.Unmarshal(data, aux); err != nil {
		return err
	}

	c.FromGoGitCommit(aux.Commit)

	return nil
}

// fill in as much of Commit as possible from the given go-git commit
func (c *Commit) FromGoGitCommit(gc *object.Commit) {
	if gc == nil {
		return
	}

	if c.Hash.IsZero() {
		c.Hash = gc.Hash
	}
	if c.This == "" {
		c.This = gc.Hash.String()
	}
	if isEmptySignature(c.Author) {
		c.Author = gc.Author
	}
	if isEmptySignature(c.Committer) {
		c.Committer = gc.Committer
	}
	if c.Message == "" {
		c.Message = gc.Message
	}
	if c.Tree == "" {
		c.Tree = gc.TreeHash.String()
	}
	if c.PGPSignature == "" {
		c.PGPSignature = gc.PGPSignature
	}
	if c.MergeTag == "" {
		c.MergeTag = gc.MergeTag
	}

	if len(c.ParentHashes) == 0 {
		c.ParentHashes = gc.ParentHashes
	}
	if c.Parent == "" && len(gc.ParentHashes) > 0 {
		c.Parent = gc.ParentHashes[0].String()
	}

	if len(c.ExtraHeaders) == 0 {
		c.ExtraHeaders = make(map[string][]byte)
		maps.Copy(c.ExtraHeaders, gc.ExtraHeaders)
	}

	if c.ChangeId == "" {
		if v, ok := gc.ExtraHeaders["change-id"]; ok {
			c.ChangeId = string(v)
		}
	}
}

func isEmptySignature(s object.Signature) bool {
	return s.Email == "" && s.Name == "" && s.When.IsZero()
}

// produce a verifiable payload from this commit's metadata
func (c *Commit) Payload() string {
	author := bytes.NewBuffer([]byte{})
	c.Author.Encode(author)

	committer := bytes.NewBuffer([]byte{})
	c.Committer.Encode(committer)

	payload := strings.Builder{}

	fmt.Fprintf(&payload, "tree %s\n", c.Tree)

	if len(c.ParentHashes) > 0 {
		for _, p := range c.ParentHashes {
			fmt.Fprintf(&payload, "parent %s\n", p.String())
		}
	} else if c.Parent != "" {
		// present for backwards compatibility
		fmt.Fprintf(&payload, "parent %s\n", c.Parent)
	}

	fmt.Fprintf(&payload, "author %s\n", author.String())
	fmt.Fprintf(&payload, "committer %s\n", committer.String())

	if c.ChangeId != "" {
		fmt.Fprintf(&payload, "change-id %s\n", c.ChangeId)
	} else if v, ok := c.ExtraHeaders["change-id"]; ok {
		fmt.Fprintf(&payload, "change-id %s\n", string(v))
	}

	fmt.Fprintf(&payload, "\n%s", c.Message)

	return payload.String()
}

var (
	coAuthorRegex = regexp.MustCompile(`(?im)^Co-authored-by:\s*(.+?)\s*<([^>]+)>`)
)

func (commit Commit) CoAuthors() []object.Signature {
	var coAuthors []object.Signature
	seen := make(map[string]bool)
	matches := coAuthorRegex.FindAllStringSubmatch(commit.Message, -1)

	for _, match := range matches {
		if len(match) >= 3 {
			name := strings.TrimSpace(match[1])
			email := strings.TrimSpace(match[2])

			if seen[email] {
				continue
			}
			seen[email] = true

			coAuthors = append(coAuthors, object.Signature{
				Name:  name,
				Email: email,
				When:  commit.Committer.When,
			})
		}
	}

	return coAuthors
}
