package models

import (
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/samber/lo"
	"tangled.org/core/api/tangled"
)

type StateValue string

const (
	StateOpen   StateValue = "open"
	StateClosed StateValue = "closed"
	StateMerged StateValue = "merged"
)

type StateRecord struct {
	Did        string
	Rkey       string
	Subject    syntax.ATURI
	Value      StateValue
	SortMicros int64
}

func sortMicros(createdAt, rkey string) int64 {
	if t, err := syntax.ParseDatetimeTime(createdAt); err == nil {
		return t.UnixMicro()
	}
	if tid, err := syntax.ParseTID(rkey); err == nil {
		return tid.Time().UnixMicro()
	}
	return 0
}

func newStateRecord(did, rkey, subjectUri, createdAt string, value StateValue) (StateRecord, error) {
	subject, err := syntax.ParseATURI(subjectUri)
	if err != nil {
		return StateRecord{}, fmt.Errorf("invalid subject uri: %w", err)
	}
	return StateRecord{
		Did:        did,
		Rkey:       rkey,
		Subject:    subject,
		Value:      value,
		SortMicros: sortMicros(createdAt, rkey),
	}, nil
}

func IssueStateFromRecord(did, rkey string, record tangled.RepoIssueState) (StateRecord, error) {
	switch record.State {
	case tangled.RepoIssueStateOpen:
		return newStateRecord(did, rkey, record.Issue, record.CreatedAt, StateOpen)
	case tangled.RepoIssueStateClosed:
		return newStateRecord(did, rkey, record.Issue, record.CreatedAt, StateClosed)
	default:
		return StateRecord{}, fmt.Errorf("unknown issue state variant: %q", record.State)
	}
}

func PullStatusFromRecord(did, rkey string, record tangled.RepoPullStatus) (StateRecord, error) {
	switch record.Status {
	case tangled.RepoPullStatusOpen:
		return newStateRecord(did, rkey, record.Pull, record.CreatedAt, StateOpen)
	case tangled.RepoPullStatusClosed:
		return newStateRecord(did, rkey, record.Pull, record.CreatedAt, StateClosed)
	case tangled.RepoPullStatusMerged:
		return newStateRecord(did, rkey, record.Pull, record.CreatedAt, StateMerged)
	default:
		return StateRecord{}, fmt.Errorf("unknown pull status variant: %q", record.Status)
	}
}

func AsIssueStateRecord(subject syntax.ATURI, value StateValue, createdAt time.Time) (tangled.RepoIssueState, error) {
	var variant string
	switch value {
	case StateOpen:
		variant = tangled.RepoIssueStateOpen
	case StateClosed:
		variant = tangled.RepoIssueStateClosed
	default:
		return tangled.RepoIssueState{}, fmt.Errorf("invalid issue state: %q", value)
	}
	return tangled.RepoIssueState{
		Issue:     subject.String(),
		State:     variant,
		CreatedAt: createdAt.UTC().Format(syntax.AtprotoDatetimeLayout),
	}, nil
}

func AsPullStatusRecords(subjects []syntax.ATURI, value StateValue, createdAt time.Time) ([]tangled.RepoPullStatus, error) {
	var variant string
	switch value {
	case StateOpen:
		variant = tangled.RepoPullStatusOpen
	case StateClosed:
		variant = tangled.RepoPullStatusClosed
	case StateMerged:
		variant = tangled.RepoPullStatusMerged
	default:
		return nil, fmt.Errorf("invalid pull status: %q", value)
	}
	created := createdAt.UTC().Format(syntax.AtprotoDatetimeLayout)
	return lo.Map(subjects, func(subject syntax.ATURI, _ int) tangled.RepoPullStatus {
		return tangled.RepoPullStatus{
			Pull:      subject.String(),
			Status:    variant,
			CreatedAt: created,
		}
	}), nil
}
