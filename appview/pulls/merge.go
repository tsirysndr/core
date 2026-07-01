package pulls

import (
	"fmt"
	"net/http"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/orm"
	"tangled.org/core/xrpc/xrpcclient"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func (s *Pulls) MergePull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "MergePull")

	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		l.Error("nil user")
		s.pages.Notice(w, "pull-action-error", "You must be logged in to merge this pull.")
		return
	}
	l = l.With("user", user.Did)

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to merge pull request. Try again later.")
		return
	}
	l = l.With("repo_at", f.RepoAt().String())

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-action-error", "Failed to merge patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "target_branch", pull.TargetBranch)

	stack, ok := r.Context().Value("stack").(models.Stack)
	if !ok {
		l.Error("failed to get stack")
		s.pages.Notice(w, "pull-action-error", "Failed to merge patch. Try again later.")
		return
	}

	// combine patches of substack
	subStack := stack.Below(pull)
	// collect the portion of the stack that is mergeable
	pullsToMerge := subStack.Mergeable()
	l = l.With("pulls_to_merge", len(pullsToMerge))

	patch := pullsToMerge.CombinedPatch()

	ident, err := s.idResolver.ResolveIdent(r.Context(), pull.OwnerDid)
	if err != nil {
		l.Error("failed to resolve identity", "err", err, "owner_did", pull.OwnerDid)
		w.WriteHeader(http.StatusNotFound)
		return
	}

	email, err := db.GetPrimaryEmail(s.db, pull.OwnerDid)
	if err != nil {
		l.Warn("failed to get primary email", "err", err, "owner_did", pull.OwnerDid)
	}

	authorName := ident.Handle.String()
	mergeInput := &tangled.RepoMerge_Input{
		Did:           f.Did,
		Name:          f.Name,
		Branch:        pull.TargetBranch,
		Patch:         patch,
		CommitMessage: &pull.Title,
		AuthorName:    &authorName,
	}

	if pull.Body != "" {
		mergeInput.CommitBody = &pull.Body
	}

	if email.Address != "" {
		mergeInput.AuthorEmail = &email.Address
	}

	client, err := s.oauth.ServiceClient(
		r,
		oauth.WithService(f.Knot),
		oauth.WithLxm(tangled.RepoMergeNSID),
		oauth.WithDev(s.config.Core.Dev),
		oauth.WithTimeout(time.Second*20), // merge is quite slow on large repos, like witchsky
	)
	if err != nil {
		l.Error("failed to connect to knot server", "err", err, "knot", f.Knot)
		s.pages.Notice(w, "pull-action-error", "Failed to merge pull request. Try again later.")
		return
	}

	err = tangled.RepoMerge(r.Context(), client, mergeInput)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		s.logger.Error("failed to merge", "xrpcerr", xrpcerr, "err", err)
		s.pages.Notice(w, "pull-action-error", xrpcerr.Error())
		return
	}

	var atUris []syntax.ATURI
	for _, p := range pullsToMerge {
		atUris = append(atUris, p.AtUri())
		p.State = models.PullMerged
	}

	if err := s.writePullStatusRecords(r, user.Did, atUris, models.StateMerged); err != nil {
		l.Error("failed to write pull status records after merge", "err", err)
	}

	tx, err := s.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to merge pull request. Try again later.")
		return
	}
	defer tx.Rollback()

	err = db.MergePulls(tx, orm.FilterEq("repo_did", string(f.RepoDid)), orm.FilterIn("at_uri", atUris))
	if err != nil {
		l.Error("failed to update pull request status in database", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to merge pull request. Try again later.")
		return
	}

	err = tx.Commit()
	if err != nil {
		// TODO: this is unsound, we should also revert the merge from the knotserver here
		l.Error("failed to commit merge transaction", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to merge pull request. Try again later.")
		return
	}

	// notify about the pull merge
	for _, p := range pullsToMerge {
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}
