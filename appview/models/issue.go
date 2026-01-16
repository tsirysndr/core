package models

import (
	"fmt"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/pages/markup/sanitizer"
)

type Issue struct {
	Id         int64
	Did        string
	Rkey       string
	RepoDid    syntax.DID
	IssueId    int
	Created    time.Time
	Edited     *time.Time
	Deleted    *time.Time
	Title      string
	Body       string
	Open       bool
	Mentions   []syntax.DID
	References []syntax.ATURI

	// optionally, populate this when querying for reverse mappings
	// like comment counts, parent repo etc.
	Comments []Comment
	Labels   LabelState
	Repo     *Repo
}

func (i *Issue) AtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", i.Did, tangled.RepoIssueNSID, i.Rkey))
}

func (i *Issue) AsRecord() tangled.RepoIssue {
	mentions := make([]string, len(i.Mentions))
	for i, did := range i.Mentions {
		mentions[i] = string(did)
	}
	references := make([]string, len(i.References))
	for i, uri := range i.References {
		references[i] = string(uri)
	}
	rec := tangled.RepoIssue{
		Repo:       string(i.RepoDid),
		Title:      i.Title,
		Body:       &i.Body,
		Mentions:   mentions,
		References: references,
		CreatedAt:  i.Created.Format(time.RFC3339),
	}
	return rec
}

func (i *Issue) State() string {
	if i.Open {
		return "open"
	}
	return "closed"
}

var _ Validator = new(Issue)

func (i *Issue) Validate() error {
	if i.Title == "" {
		return fmt.Errorf("issue title is empty")
	}
	if i.Body == "" {
		return fmt.Errorf("issue body is empty")
	}

	if st := strings.TrimSpace(sanitizer.SanitizeDescription(i.Title)); st == "" {
		return fmt.Errorf("title is empty after HTML sanitization")
	}

	if st := strings.TrimSpace(sanitizer.SanitizeDefault(i.Body)); st == "" {
		return fmt.Errorf("body is empty after HTML sanitization")
	}
	return nil
}

func (i *Issue) Participants() []syntax.DID {
	participantSet := make(map[syntax.DID]struct{})
	participants := []syntax.DID{}

	addParticipant := func(did syntax.DID) {
		if _, exists := participantSet[did]; !exists {
			participantSet[did] = struct{}{}
			participants = append(participants, did)
		}
	}

	addParticipant(syntax.DID(i.Did))

	for _, c := range i.Comments {
		addParticipant(c.Did)
	}

	return participants
}

func IssueFromRecord(did, rkey string, record tangled.RepoIssue) Issue {
	created, err := time.Parse(time.RFC3339, record.CreatedAt)
	if err != nil {
		created = time.Now()
	}

	body := ""
	if record.Body != nil {
		body = *record.Body
	}

	return Issue{
		RepoDid: syntax.DID(record.Repo),
		Did:     did,
		Rkey:    rkey,
		Created: created,
		Title:   record.Title,
		Body:    body,
		Open:    true, // new issues are open by default
	}
}
