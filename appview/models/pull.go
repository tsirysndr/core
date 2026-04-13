package models

import (
	"bytes"
	"compress/gzip"
	"fmt"
	"io"
	"log"
	"slices"
	"strings"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/patchutil"
	"tangled.org/core/types"

	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
)

type PullState int

const (
	PullClosed PullState = iota
	PullOpen
	PullMerged
	PullAbandoned
)

func (p PullState) String() string {
	switch p {
	case PullOpen:
		return "open"
	case PullMerged:
		return "merged"
	case PullClosed:
		return "closed"
	case PullAbandoned:
		return "abandoned"
	default:
		return "closed"
	}
}

func (p PullState) IsOpen() bool {
	return p == PullOpen
}
func (p PullState) IsMerged() bool {
	return p == PullMerged
}
func (p PullState) IsClosed() bool {
	return p == PullClosed
}
func (p PullState) IsAbandoned() bool {
	return p == PullAbandoned
}

type Pull struct {
	// ids
	ID     int
	PullId int

	// at ids
	RepoAt   syntax.ATURI
	OwnerDid string
	Rkey     string

	// content
	Title        string
	Body         string
	TargetBranch string
	State        PullState
	Submissions  []*PullSubmission
	Mentions     []syntax.DID
	References   []syntax.ATURI

	// stacking
	DependentOn *syntax.ATURI

	// meta
	Created    time.Time
	PullSource *PullSource

	// optionally, populate this when querying for reverse mappings
	Labels LabelState
	Repo   *Repo
}

// NOTE: This method does not include patch blob in returned atproto record
func (p Pull) AsRecord() tangled.RepoPull {
	mentions := make([]string, len(p.Mentions))
	for i, did := range p.Mentions {
		mentions[i] = string(did)
	}
	references := make([]string, len(p.References))
	for i, uri := range p.References {
		references[i] = string(uri)
	}

	var targetRepoAt, targetRepoDid *string
	targetRepoAt = new(string)
	*targetRepoAt = p.RepoAt.String()
	if p.Repo != nil && p.Repo.RepoDid != "" {
		targetRepoDid = new(string)
		*targetRepoDid = p.Repo.RepoDid
	}

	rounds := make([]*tangled.RepoPull_Round, len(p.Submissions))
	for i, submission := range p.Submissions {
		rounds[i] = submission.AsRecord()
	}

	var dependentOn *string
	if p.DependentOn != nil {
		x := p.DependentOn.String()
		dependentOn = &x
	}

	return tangled.RepoPull{
		Title:      p.Title,
		Body:       &p.Body,
		Mentions:   mentions,
		References: references,
		CreatedAt:  p.Created.Format(time.RFC3339),
		Target: &tangled.RepoPull_Target{
			Repo:    targetRepoAt,
			RepoDid: targetRepoDid,
			Branch:  p.TargetBranch,
		},
		Rounds:      rounds,
		Source:      p.PullSource.AsRecord(),
		DependentOn: dependentOn,
	}
}

func PullFromRecord(did, rkey string, record tangled.RepoPull, blobs []*io.ReadCloser) (*Pull, error) {
	created, err := time.Parse(time.RFC3339, record.CreatedAt)
	if err != nil {
		return nil, fmt.Errorf("invalid createdAt: %w", err)
	}

	body := ""
	if record.Body != nil {
		body = *record.Body
	}

	var mentions []syntax.DID
	for _, m := range record.Mentions {
		if did, err := syntax.ParseDID(m); err == nil {
			mentions = append(mentions, did)
		}
	}

	var targetRepoAt syntax.ATURI
	var targetBranch string
	if record.Target != nil {
		if record.Target.Repo != nil {
			uri, err := syntax.ParseATURI(*record.Target.Repo)
			if err != nil {
				return nil, fmt.Errorf("invalid target.repo aturi: %w", err)
			}
			targetRepoAt = uri
		}
		targetBranch = record.Target.Branch
	}

	var pullSource *PullSource
	if record.Source != nil {
		pullSource = &PullSource{
			Branch: record.Source.Branch,
		}

		if record.Source.Repo != nil {
			uri, err := syntax.ParseATURI(*record.Source.Repo)
			if err != nil {
				return nil, fmt.Errorf("invalid source.repo aturi: %w", err)
			}
			pullSource.RepoAt = &uri
		}
		if record.Source.RepoDid != nil {
			did, err := syntax.ParseDID(*record.Source.RepoDid)
			if err != nil {
				return nil, fmt.Errorf("invalid source.repoDid did: %w", err)
			}
			pullSource.RepoDid = &did
		}
	}

	var dependentOn *syntax.ATURI
	if record.DependentOn != nil {
		uri, err := syntax.ParseATURI(*record.DependentOn)
		if err != nil {
			return nil, fmt.Errorf("invalid dependentOn aturi: %w", err)
		}
		dependentOn = &uri
	}

	var submissions []*PullSubmission
	for i, s := range record.Rounds {
		var blob *io.ReadCloser
		if i < len(blobs) {
			blob = blobs[i]
		}
		submission, err := PullSubmissionFromRecord(did, rkey, i, s, blob)
		if err != nil {
			return nil, fmt.Errorf("invalid pull round at index %d: %w", i, err)
		}
		submissions = append(submissions, submission)
	}

	return &Pull{
		RepoAt:       targetRepoAt,
		OwnerDid:     did,
		Rkey:         rkey,
		Title:        record.Title,
		Body:         body,
		TargetBranch: targetBranch,
		PullSource:   pullSource,
		State:        PullOpen,
		Submissions:  submissions,
		Created:      created,
		DependentOn:  dependentOn,
	}, nil
}

func PullSubmissionFromRecord(did, rkey string, roundNumber int, round *tangled.RepoPull_Round, blob *io.ReadCloser) (*PullSubmission, error) {
	created, err := time.Parse(time.RFC3339, round.CreatedAt)
	if err != nil {
		return nil, fmt.Errorf("invalid createdAt: %w", err)
	}

	var patch, sourceRev string
	if blob != nil {
		p, err := extractGzip(*blob)
		if err != nil {
			return nil, fmt.Errorf("failed to extract gzip: %w", err)
		}
		patch = p
		if patchutil.IsFormatPatch(p) {
			patches, err := patchutil.ExtractPatches(p)
			if err != nil {
				return nil, fmt.Errorf("failed to extract patches: %w", err)
			}

			for _, part := range patches {
				sourceRev = part.SHA
			}
		}
	}

	return &PullSubmission{
		PullAt:      syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", did, tangled.RepoPullNSID, rkey)),
		RoundNumber: roundNumber,
		Blob:        *round.PatchBlob,
		Created:     created,
		Patch:       patch,
		SourceRev:   sourceRev,
	}, nil
}

type PullSource struct {
	Branch  string
	RepoAt  *syntax.ATURI
	RepoDid *syntax.DID

	// optionally populate this for reverse mappings
	Repo *Repo
}

func (s *PullSource) AsRecord() *tangled.RepoPull_Source {
	if s == nil {
		return nil
	}
	var repoAt, repoDid *string
	if s.RepoAt != nil {
		repoAt = new(string)
		*repoAt = s.RepoAt.String()
	}
	if s.RepoDid != nil {
		repoDid = new(string)
		*repoDid = s.RepoDid.String()
	}
	return &tangled.RepoPull_Source{
		Branch:  s.Branch,
		Repo:    repoAt,
		RepoDid: repoDid,
	}
}

type PullSubmission struct {
	// ids
	ID int

	// at ids
	PullAt syntax.ATURI

	// content
	RoundNumber int
	Blob        lexutil.LexBlob
	Patch       string
	Combined    string
	Comments    []PullComment
	SourceRev   string // include the rev that was used to create this submission: only for branch/fork PRs

	// meta
	Created time.Time
}

type PullComment struct {
	// ids
	ID           int
	PullId       int
	SubmissionId int

	// at ids
	RepoAt    string
	OwnerDid  string
	CommentAt string

	// content
	Body string

	// meta
	Mentions   []syntax.DID
	References []syntax.ATURI

	// meta
	Created time.Time
}

func (p *PullComment) AtUri() syntax.ATURI {
	return syntax.ATURI(p.CommentAt)
}

func (p *Pull) TotalComments() int {
	total := 0
	for _, s := range p.Submissions {
		total += len(s.Comments)
	}
	return total
}

func (p *Pull) LastRoundNumber() int {
	return len(p.Submissions) - 1
}

func (p *Pull) LatestSubmission() *PullSubmission {
	return p.Submissions[p.LastRoundNumber()]
}

func (p *Pull) LatestPatch() string {
	return p.LatestSubmission().Patch
}

func (p *Pull) LatestSha() string {
	return p.LatestSubmission().SourceRev
}

func (p *Pull) AtUri() syntax.ATURI {
	return syntax.ATURI(fmt.Sprintf("at://%s/%s/%s", p.OwnerDid, tangled.RepoPullNSID, p.Rkey))
}

func (p *Pull) IsPatchBased() bool {
	return p.PullSource == nil
}

func (p *Pull) IsBranchBased() bool {
	if p.PullSource != nil {
		if p.PullSource.RepoAt != nil {
			return p.PullSource.RepoAt == &p.RepoAt
		} else {
			// no repo specified
			return true
		}
	}
	return false
}

func (p *Pull) IsForkBased() bool {
	if p.PullSource != nil {
		if p.PullSource.RepoAt != nil {
			// make sure repos are different
			return p.PullSource.RepoAt != &p.RepoAt
		}
	}
	return false
}

func (p *Pull) Participants() []string {
	participantSet := make(map[string]struct{})
	participants := []string{}

	addParticipant := func(did string) {
		if _, exists := participantSet[did]; !exists {
			participantSet[did] = struct{}{}
			participants = append(participants, did)
		}
	}

	addParticipant(p.OwnerDid)

	for _, s := range p.Submissions {
		for _, sp := range s.Participants() {
			addParticipant(sp)
		}
	}

	return participants
}

func (s PullSubmission) IsFormatPatch() bool {
	return patchutil.IsFormatPatch(s.Patch)
}

func (s PullSubmission) AsFormatPatch() []types.FormatPatch {
	patches, err := patchutil.ExtractPatches(s.Patch)
	if err != nil {
		log.Println("error extracting patches from submission:", err)
		return []types.FormatPatch{}
	}

	return patches
}

// empty if invalid, not otherwise
func (s PullSubmission) ChangeId() string {
	patches := s.AsFormatPatch()
	if len(patches) != 1 {
		return ""
	}

	c, err := patches[0].ChangeId()
	if err != nil {
		return ""
	}

	return c
}

func (s *PullSubmission) Participants() []string {
	participantSet := make(map[string]struct{})
	participants := []string{}

	addParticipant := func(did string) {
		if _, exists := participantSet[did]; !exists {
			participantSet[did] = struct{}{}
			participants = append(participants, did)
		}
	}

	addParticipant(s.PullAt.Authority().String())

	for _, c := range s.Comments {
		addParticipant(c.OwnerDid)
	}

	return participants
}

func (s PullSubmission) CombinedPatch() string {
	if s.Combined == "" {
		return s.Patch
	}

	return s.Combined
}

func (s *PullSubmission) GetBlob() *lexutil.LexBlob {
	if !s.Blob.Ref.Defined() {
		return nil
	}

	return &s.Blob
}

func (s *PullSubmission) AsRecord() *tangled.RepoPull_Round {
	return &tangled.RepoPull_Round{
		CreatedAt: s.Created.Format(time.RFC3339),
		PatchBlob: s.GetBlob(),
	}
}

type Stack []*Pull

// position of this pull in the stack
func (stack Stack) Position(pull *Pull) int {
	return slices.IndexFunc(stack, func(p *Pull) bool {
		return p.AtUri() == pull.AtUri()
	})
}

// all pulls below this pull (including self) in this stack
//
// nil if this pull does not belong to this stack
func (stack Stack) Below(pull *Pull) Stack {
	position := stack.Position(pull)

	if position < 0 {
		return nil
	}

	return stack[position:]
}

// all pulls below this pull (excluding self) in this stack
func (stack Stack) StrictlyBelow(pull *Pull) Stack {
	below := stack.Below(pull)

	if len(below) > 0 {
		return below[1:]
	}

	return nil
}

// all pulls above this pull (including self) in this stack
func (stack Stack) Above(pull *Pull) Stack {
	position := stack.Position(pull)

	if position < 0 {
		return nil
	}

	return stack[:position+1]
}

// all pulls below this pull (excluding self) in this stack
func (stack Stack) StrictlyAbove(pull *Pull) Stack {
	above := stack.Above(pull)

	if len(above) > 0 {
		return above[:len(above)-1]
	}

	return nil
}

// the combined format-patches of all the newest submissions in this stack
func (stack Stack) CombinedPatch() string {
	// go in reverse order because the bottom of the stack is the last element in the slice
	var combined strings.Builder
	for idx := range stack {
		pull := stack[len(stack)-1-idx]
		combined.WriteString(pull.LatestPatch())
		combined.WriteString("\n")
	}
	return combined.String()
}

// filter out PRs that are "active"
//
// PRs that are still open are active
func (stack Stack) Mergeable() Stack {
	var mergeable Stack

	for _, p := range stack {
		// stop at the first merged PR
		if p.State == PullMerged || p.State == PullClosed {
			break
		}

		// skip over abandoned PRs
		if p.State != PullAbandoned {
			mergeable = append(mergeable, p)
		}
	}

	return mergeable
}

type BranchDeleteStatus struct {
	Repo   *Repo
	Branch string
}

func extractGzip(blob io.Reader) (string, error) {
	var b bytes.Buffer
	r, err := gzip.NewReader(blob)
	if err != nil {
		return "", err
	}
	defer r.Close()

	const maxSize = 15 * 1024 * 1024
	limitedReader := io.LimitReader(r, maxSize)

	_, err = io.Copy(&b, limitedReader)
	if err != nil {
		return "", err
	}

	return b.String(), nil
}
