package migration

import (
	"context"
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
	// TODO: use agnostic.RepoGetRecord instead
	ex, err := comatproto.RepoGetRecord(ctx, client, "", record.Collection().String(), did.String(), record.RecordKey().String())
	if err != nil {
		return fmt.Errorf("pds: %w", err)
	}

	val := ex.Value.Val

	switch record.Collection() {
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
		if rec.Repo != nil {
			repoAt := *rec.Repo
			repo, err := db.GetRepoByAtUri(s.db, repoAt)
			if err != nil {
				return fmt.Errorf("db: failed to query repo: %w", err)
			}
			rec.RepoDid = &repo.RepoDid
		}

	case tangled.RepoPullNSID:
		rec, ok := val.(*tangled.RepoPull)
		if !ok {
			return fmt.Errorf("unexpected type for pull record")
		}
		if rec.Target != nil && rec.Target.Repo != nil {
			repoAt := *rec.Target.Repo
			repo, err := db.GetRepoByAtUri(s.db, repoAt)
			if err != nil {
				return fmt.Errorf("db: failed to query repo: %w", err)
			}
			rec.Target.RepoDid = &repo.RepoDid
		}
		if rec.Source != nil && rec.Source.Repo != nil {
			repoAt := *rec.Source.Repo
			repo, err := db.GetRepoByAtUri(s.db, repoAt)
			if err != nil {
				return fmt.Errorf("db: failed to query repo: %w", err)
			}
			rec.Source.RepoDid = &repo.RepoDid
		}

	case tangled.RepoCollaboratorNSID:
		rec, ok := val.(*tangled.RepoCollaborator)
		if !ok {
			return fmt.Errorf("unexpected type for collaborator record")
		}
		if rec.Repo != nil {
			repoAt := *rec.Repo
			repo, err := db.GetRepoByAtUri(s.db, repoAt)
			if err != nil {
				return fmt.Errorf("db: failed to query repo: %w", err)
			}
			rec.RepoDid = &repo.RepoDid
		}

	case tangled.RepoArtifactNSID:
		rec, ok := val.(*tangled.RepoArtifact)
		if !ok {
			return fmt.Errorf("unexpected type for artifact record")
		}
		if rec.Repo != nil {
			repoAt := *rec.Repo
			repo, err := db.GetRepoByAtUri(s.db, repoAt)
			if err != nil {
				return fmt.Errorf("db: failed to query repo: %w", err)
			}
			rec.RepoDid = &repo.RepoDid
		}

	case tangled.FeedStarNSID:
		rec, ok := val.(*tangled.FeedStar)
		if !ok {
			return fmt.Errorf("unexpected type for star record")
		}
		if rec.Subject != nil {
			repoAt := *rec.Subject
			repo, err := db.GetRepoByAtUri(s.db, repoAt)
			if err != nil {
				return fmt.Errorf("db: failed to query repo: %w", err)
			}
			rec.SubjectDid = &repo.RepoDid
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
