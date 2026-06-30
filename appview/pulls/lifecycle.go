package pulls

import (
	"fmt"
	"net/http"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/orm"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

func (s *Pulls) ClosePull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ClosePull")

	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		l.Error("nil user")
		s.pages.Notice(w, "pull-action-error", "You must be logged in to close this pull.")
		return
	}
	l = l.With("user", user.Did)

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-action-error", "Failed to close pull. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	// auth filter: only owner or collaborators can close
	roles := s.acl.RolesInRepo(r.Context(), f, user.Did)
	isOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isPullAuthor := user.Did == pull.OwnerDid
	isCloseAllowed := isOwner || isCollaborator || isPullAuthor
	if !isCloseAllowed {
		l.Error("unauthorized to close pull", "is_owner", isOwner, "is_collaborator", isCollaborator, "is_pull_author", isPullAuthor)
		s.pages.Notice(w, "pull-action-error", "You are unauthorized to close this pull.")
		return
	}

	// if this PR is stacked, then we want to close all PRs above this one on the stack
	stack := r.Context().Value("stack").(models.Stack)
	pullsToClose := stack.Above(pull)
	var atUris []syntax.ATURI
	for _, p := range pullsToClose {
		atUris = append(atUris, p.AtUri())
		p.State = models.PullClosed
	}

	if err := s.writePullStatusRecords(r, user.Did, atUris, models.StateClosed); err != nil {
		l.Error("failed to write pull status records", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to close pull. Try again later.")
		return
	}

	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to close pull.")
		return
	}
	defer tx.Rollback()

	err = db.ClosePulls(
		tx,
		orm.FilterEq("repo_did", string(f.RepoDid)),
		orm.FilterIn("at_uri", atUris),
	)
	if err != nil {
		l.Error("failed to close pulls in database", "err", err, "pulls_to_close", len(pullsToClose))
		s.pages.Notice(w, "pull-action-error", "Failed to close pull.")
		return
	}

	// Commit the transaction
	if err = tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to close pull.")
		return
	}

	for _, p := range pullsToClose {
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}

func (s *Pulls) ReopenPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "ReopenPull")

	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		l.Error("nil user")
		s.pages.Notice(w, "pull-action-error", "You must be logged in to reopen this pull.")
		return
	}
	l = l.With("user", user.Did)

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve repo", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to reopen pull.")
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-action-error", "Failed to reopen pull. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid, "state", pull.State)

	// auth filter: only owner or collaborators can close
	roles := s.acl.RolesInRepo(r.Context(), f, user.Did)
	isOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isPullAuthor := user.Did == pull.OwnerDid
	isCloseAllowed := isOwner || isCollaborator || isPullAuthor
	if !isCloseAllowed {
		l.Error("unauthorized to reopen pull", "is_owner", isOwner, "is_collaborator", isCollaborator, "is_pull_author", isPullAuthor)
		s.pages.Notice(w, "pull-action-error", "You are unauthorized to reopen this pull.")
		return
	}

	// if this PR is stacked, then we want to reopen all PRs above this one on the stack
	stack := r.Context().Value("stack").(models.Stack)
	pullsToReopen := stack.Below(pull)
	var atUris []syntax.ATURI
	for _, p := range pullsToReopen {
		atUris = append(atUris, p.AtUri())
		p.State = models.PullOpen
	}

	if err := s.writePullStatusRecords(r, user.Did, atUris, models.StateOpen); err != nil {
		l.Error("failed to write pull status records", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to reopen pull. Try again later.")
		return
	}

	tx, err := s.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to reopen pull.")
		return
	}
	defer tx.Rollback()

	err = db.ReopenPulls(
		tx,
		orm.FilterEq("repo_did", string(f.RepoDid)),
		orm.FilterIn("at_uri", atUris),
	)
	if err != nil {
		l.Error("failed to reopen pulls in database", "err", err, "pulls_to_reopen", len(pullsToReopen))
		s.pages.Notice(w, "pull-action-error", "Failed to reopen pull.")
		return
	}

	// Commit the transaction
	if err = tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		s.pages.Notice(w, "pull-action-error", "Failed to reopen pull.")
		return
	}

	for _, p := range pullsToReopen {
		s.notifier.NewPullState(r.Context(), syntax.DID(user.Did), p)
	}

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	s.pages.HxLocation(w, fmt.Sprintf("/%s/pulls/%d", ownerSlashRepo, pull.PullId))
}
