package migration

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
)

func (s *Migration) migrateAddRepoDid(ctx context.Context, client *atclient.APIClient, did syntax.DID, record syntax.ATURI) error {
	if record.Collection().String() == tangled.FeedStarNSID {
		return s.migrateAddRepoDidStar(ctx, client, did, record)
	}

	ex, err := comatproto.RepoGetRecord(ctx, client, "", record.Collection().String(), did.String(), record.RecordKey().String())
	if err != nil {
		return fmt.Errorf("pds: %w", err)
	}

	val := ex.Value.Val

	switch record.Collection().String() {
	case tangled.RepoNSID:
		rec, ok := val.(*tangled.Repo)
		if !ok {
			return fmt.Errorf("unexpected type for repo record")
		}
		repo, err := db.GetRepoByAtUri(s.db, record.String())
		if err != nil {
			return fmt.Errorf("db: failed to query repo: %w", err)
		}
		rec.RepoDid = &repo.RepoDid

	case tangled.RepoIssueNSID:
		rec, ok := val.(*tangled.RepoIssue)
		if !ok {
			return fmt.Errorf("unexpected type for issue record")
		}
		if strings.HasPrefix(rec.Repo, "did:") {
			return nil
		}
		repo, err := db.GetRepoByAtUri(s.db, rec.Repo)
		if err != nil {
			return fmt.Errorf("db: failed to query repo by at_uri %q: %w", rec.Repo, err)
		}
		rec.Repo = repo.RepoDid

	case tangled.RepoPullNSID:
		rec, ok := val.(*tangled.RepoPull)
		if !ok {
			return fmt.Errorf("unexpected type for pull record")
		}
		if rec.Target == nil {
			return fmt.Errorf("pull record has nil target")
		}
		if !strings.HasPrefix(rec.Target.Repo, "did:") {
			repo, err := db.GetRepoByAtUri(s.db, rec.Target.Repo)
			if err != nil {
				return fmt.Errorf("db: failed to query target repo by at_uri %q: %w", rec.Target.Repo, err)
			}
			rec.Target.Repo = repo.RepoDid
		}
		if rec.Source != nil && rec.Source.Repo != nil && !strings.HasPrefix(*rec.Source.Repo, "did:") {
			sourceRepo, srcErr := db.GetRepoByAtUri(s.db, *rec.Source.Repo)
			if srcErr == nil && sourceRepo.RepoDid != "" {
				rec.Source.Repo = &sourceRepo.RepoDid
			}
		}

	case tangled.RepoCollaboratorNSID:
		rec, ok := val.(*tangled.RepoCollaborator)
		if !ok {
			return fmt.Errorf("unexpected type for collaborator record")
		}
		if strings.HasPrefix(rec.Repo, "did:") {
			return nil
		}
		repo, err := db.GetRepoByAtUri(s.db, rec.Repo)
		if err != nil {
			return fmt.Errorf("db: failed to query repo by at_uri %q: %w", rec.Repo, err)
		}
		rec.Repo = repo.RepoDid

	case tangled.RepoArtifactNSID:
		rec, ok := val.(*tangled.RepoArtifact)
		if !ok {
			return fmt.Errorf("unexpected type for artifact record")
		}
		if rec.Repo != nil {
			repo, err := db.GetRepoByAtUri(s.db, *rec.Repo)
			if err != nil {
				return fmt.Errorf("db: failed to query repo by at_uri %q: %w", *rec.Repo, err)
			}
			rec.RepoDid = &repo.RepoDid
		}

	case tangled.ActorProfileNSID:
		rec, ok := val.(*tangled.ActorProfile)
		if !ok {
			return fmt.Errorf("unexpected type for profile record")
		}
		rewritten := make([]string, 0, len(rec.PinnedRepositories))
		for _, pin := range rec.PinnedRepositories {
			if strings.HasPrefix(pin, "did:") {
				rewritten = append(rewritten, pin)
				continue
			}
			repo, repoErr := db.GetRepoByAtUri(s.db, pin)
			if repoErr != nil || repo.RepoDid == "" {
				rewritten = append(rewritten, pin)
				continue
			}
			rewritten = append(rewritten, repo.RepoDid)
		}
		rec.PinnedRepositories = rewritten

	default:
		return fmt.Errorf("unexpected collection: '%s'", record.Collection())
	}

	_, err = comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
		Repo:       did.String(),
		Collection: record.Collection().String(),
		Rkey:       record.RecordKey().String(),
		SwapRecord: ex.Cid,
		Record:     &lexutil.LexiconTypeDecoder{Val: val},
	})
	if err != nil {
		return fmt.Errorf("put record: %w", err)
	}

	return nil
}

func (s *Migration) migrateAddRepoDidStar(ctx context.Context, client *atclient.APIClient, did syntax.DID, record syntax.ATURI) error {
	var raw struct {
		Cid   *string         `json:"cid,omitempty"`
		Uri   string          `json:"uri"`
		Value json.RawMessage `json:"value"`
	}
	params := map[string]any{
		"collection": record.Collection().String(),
		"repo":       did.String(),
		"rkey":       record.RecordKey().String(),
	}
	if err := client.LexDo(ctx, lexutil.Query, "", "com.atproto.repo.getRecord", params, nil, &raw); err != nil {
		return fmt.Errorf("get record: %w", err)
	}

	var legacy struct {
		CreatedAt string  `json:"createdAt"`
		Subject   *string `json:"subject,omitempty"`
	}
	if err := json.Unmarshal(raw.Value, &legacy); err != nil {
		return fmt.Errorf("decode old star fields: %w", err)
	}
	if legacy.Subject == nil {
		return fmt.Errorf("star record has no subject field")
	}

	repo, err := db.GetRepoByAtUri(s.db, *legacy.Subject)
	if err != nil {
		return fmt.Errorf("db: failed to query repo by at_uri %q: %w", *legacy.Subject, err)
	}
	if repo.RepoDid == "" {
		return fmt.Errorf("repo has no repoDid: %s", *legacy.Subject)
	}

	newRecord := &tangled.FeedStar{
		CreatedAt: legacy.CreatedAt,
		Subject: &tangled.FeedStar_Subject{
			FeedStar_Repo: &tangled.FeedStar_Repo{Did: repo.RepoDid},
		},
	}

	_, err = comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
		Repo:       did.String(),
		Collection: record.Collection().String(),
		Rkey:       record.RecordKey().String(),
		SwapRecord: raw.Cid,
		Record:     &lexutil.LexiconTypeDecoder{Val: newRecord},
	})
	if err != nil {
		return fmt.Errorf("put record: %w", err)
	}
	return nil
}
