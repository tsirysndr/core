package issues

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"github.com/go-chi/chi/v5"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	issues_indexer "tangled.org/core/appview/indexer/issues"
	"tangled.org/core/appview/mentions"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pages/repoinfo"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/searchquery"
	"tangled.org/core/appview/validator"
	"tangled.org/core/idresolver"
	"tangled.org/core/ogre"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/tid"
)

type Issues struct {
	oauth            *oauth.OAuth
	repoResolver     *reporesolver.RepoResolver
	enforcer         *rbac.Enforcer
	pages            *pages.Pages
	idResolver       *idresolver.Resolver
	mentionsResolver *mentions.Resolver
	db               *db.DB
	config           *config.Config
	notifier         notify.Notifier
	logger           *slog.Logger
	validator        *validator.Validator
	indexer          *issues_indexer.Indexer
	ogreClient       *ogre.Client
}

func New(
	oauth *oauth.OAuth,
	repoResolver *reporesolver.RepoResolver,
	enforcer *rbac.Enforcer,
	pages *pages.Pages,
	idResolver *idresolver.Resolver,
	mentionsResolver *mentions.Resolver,
	db *db.DB,
	config *config.Config,
	notifier notify.Notifier,
	validator *validator.Validator,
	indexer *issues_indexer.Indexer,
	logger *slog.Logger,
) *Issues {
	return &Issues{
		oauth:            oauth,
		repoResolver:     repoResolver,
		enforcer:         enforcer,
		pages:            pages,
		idResolver:       idResolver,
		mentionsResolver: mentionsResolver,
		db:               db,
		config:           config,
		notifier:         notifier,
		logger:           logger,
		validator:        validator,
		indexer:          indexer,
		ogreClient:       ogre.NewClient(config.Ogre.Host),
	}
}

func (rp *Issues) RepoSingleIssue(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoSingleIssue")
	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	reactionMap, err := db.GetReactionMap(rp.db, 20, issue.AtUri())
	if err != nil {
		l.Error("failed to get issue reactions", "err", err)
	}

	userReactions := map[models.ReactionKind]bool{}
	if user != nil {
		userReactions = db.GetReactionStatusMap(rp.db, user.Did, issue.AtUri())
	}

	backlinks, err := db.GetBacklinks(rp.db, issue.AtUri())
	if err != nil {
		l.Error("failed to fetch backlinks", "err", err)
		rp.pages.Error503(w)
		return
	}

	labelDefs, err := db.GetLabelDefinitions(
		rp.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", tangled.RepoIssueNSID),
	)
	if err != nil {
		l.Error("failed to fetch labels", "err", err)
		rp.pages.Error503(w)
		return
	}

	vouchRelationships := make(map[syntax.DID]*models.VouchRelationship)
	if user != nil {
		participants := issue.Participants()
		vouchRelationships, err = db.GetVouchRelationshipsBatch(rp.db, syntax.DID(user.Did), participants)
		if err != nil {
			l.Error("failed to fetch vouch relationships", "err", err)
		}
	}

	defs := make(map[string]*models.LabelDefinition)
	for _, l := range labelDefs {
		defs[l.AtUri().String()] = &l
	}

	err = rp.pages.RepoSingleIssue(w, pages.RepoSingleIssueParams{
		LoggedInUser:       user,
		RepoInfo:           rp.repoResolver.GetRepoInfo(r, user),
		Issue:              issue,
		CommentList:        issue.CommentList(),
		Backlinks:          backlinks,
		Reactions:          reactionMap,
		UserReacted:        userReactions,
		LabelDefs:          defs,
		VouchRelationships: vouchRelationships,
	})
	if err != nil {
		l.Error("failed to render issue", "err", err)
	}
}

func (rp *Issues) EditIssue(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "EditIssue")
	user := rp.oauth.GetMultiAccountUser(r)

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	switch r.Method {
	case http.MethodGet:
		rp.pages.EditIssueFragment(w, pages.EditIssueParams{
			LoggedInUser: user,
			RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
			Issue:        issue,
		})
	case http.MethodPost:
		noticeId := "issues"
		newIssue := issue
		newIssue.Title = r.FormValue("title")
		newIssue.Body = r.FormValue("body")
		newIssue.Mentions, newIssue.References = rp.mentionsResolver.Resolve(r.Context(), newIssue.Body)

		if err := rp.validator.ValidateIssue(newIssue); err != nil {
			l.Error("validation error", "err", err)
			rp.pages.Notice(w, noticeId, fmt.Sprintf("Failed to edit issue: %s", err))
			return
		}

		newRecord := newIssue.AsRecord()

		// edit an atproto record
		client, err := rp.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			rp.pages.Notice(w, noticeId, "Failed to edit issue.")
			return
		}

		ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoIssueNSID, user.Did, newIssue.Rkey)
		if err != nil {
			l.Error("failed to get record", "err", err)
			rp.pages.Notice(w, noticeId, "Failed to edit issue, no record found on PDS.")
			return
		}

		_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoIssueNSID,
			Repo:       user.Did,
			Rkey:       newIssue.Rkey,
			SwapRecord: ex.Cid,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &newRecord,
			},
		})
		if err != nil {
			l.Error("failed to edit record on PDS", "err", err)
			rp.pages.Notice(w, noticeId, "Failed to edit issue on PDS.")
			return
		}

		// modify on DB -- TODO: transact this cleverly
		tx, err := rp.db.Begin()
		if err != nil {
			l.Error("failed to edit issue on DB", "err", err)
			rp.pages.Notice(w, noticeId, "Failed to edit issue.")
			return
		}
		defer tx.Rollback()

		err = db.PutIssue(tx, newIssue)
		if err != nil {
			l.Error("failed to edit issue", "err", err)
			rp.pages.Notice(w, "issues", "Failed to edit issue.")
			return
		}

		if err = tx.Commit(); err != nil {
			l.Error("failed to edit issue", "err", err)
			rp.pages.Notice(w, "issues", "Failed to cedit issue.")
			return
		}

		rp.pages.HxRefresh(w)
	}
}

func (rp *Issues) DeleteIssue(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DeleteIssue")
	noticeId := "issue-actions-error"

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Notice(w, noticeId, "Failed to delete issue.")
		return
	}
	l = l.With("did", issue.Did, "rkey", issue.Rkey)

	tx, err := rp.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment, try again later.")
		return
	}
	defer tx.Rollback()

	// delete from PDS
	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to delete comment.")
		return
	}
	_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
		Collection: tangled.RepoIssueNSID,
		Repo:       issue.Did,
		Rkey:       issue.Rkey,
	})
	if err != nil {
		// TODO: transact this better
		l.Error("failed to delete issue from PDS", "err", err)
		rp.pages.Notice(w, noticeId, "Failed to delete issue.")
		return
	}

	// delete from db
	if err := db.DeleteIssues(tx, issue.Did, issue.Rkey); err != nil {
		l.Error("failed to delete issue", "err", err)
		rp.pages.Notice(w, noticeId, "Failed to delete issue.")
		return
	}
	tx.Commit()

	rp.notifier.DeleteIssue(r.Context(), issue)

	// return to all issues page
	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	rp.pages.HxRedirect(w, "/"+ownerSlashRepo+"/issues")
}

func (rp *Issues) CloseIssue(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "CloseIssue")
	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	roles := repoinfo.RolesInRepo{Roles: rp.enforcer.GetPermissionsInRepo(user.Did, f.Knot, f.RepoIdentifier())}
	isRepoOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isIssueOwner := user.Did == issue.Did

	// TODO: make this more granular
	if isIssueOwner || isRepoOwner || isCollaborator {
		err = db.CloseIssues(
			rp.db,
			orm.FilterEq("id", issue.Id),
		)
		if err != nil {
			l.Error("failed to close issue", "err", err)
			rp.pages.Notice(w, "issue-action", "Failed to close issue. Try again later.")
			return
		}
		// change the issue state (this will pass down to the notifiers)
		issue.Open = false

		// notify about the issue closure
		rp.notifier.NewIssueState(r.Context(), syntax.DID(user.Did), issue)

		ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
		rp.pages.HxLocation(w, fmt.Sprintf("/%s/issues/%d", ownerSlashRepo, issue.IssueId))
		return
	} else {
		l.Error("user is not permitted to close issue")
		http.Error(w, "for biden", http.StatusUnauthorized)
		return
	}
}

func (rp *Issues) ReopenIssue(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "ReopenIssue")
	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	roles := repoinfo.RolesInRepo{Roles: rp.enforcer.GetPermissionsInRepo(user.Did, f.Knot, f.RepoIdentifier())}
	isRepoOwner := roles.IsOwner()
	isCollaborator := roles.IsCollaborator()
	isIssueOwner := user.Did == issue.Did

	if isCollaborator || isRepoOwner || isIssueOwner {
		err := db.ReopenIssues(
			rp.db,
			orm.FilterEq("id", issue.Id),
		)
		if err != nil {
			l.Error("failed to reopen issue", "err", err)
			rp.pages.Notice(w, "issue-action", "Failed to reopen issue. Try again later.")
			return
		}
		// change the issue state (this will pass down to the notifiers)
		issue.Open = true

		// notify about the issue reopen
		rp.notifier.NewIssueState(r.Context(), syntax.DID(user.Did), issue)

		ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
		rp.pages.HxLocation(w, fmt.Sprintf("/%s/issues/%d", ownerSlashRepo, issue.IssueId))
		return
	} else {
		l.Error("user is not the owner of the repo")
		http.Error(w, "forbidden", http.StatusUnauthorized)
		return
	}
}

func (rp *Issues) NewIssueComment(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "NewIssueComment")
	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	body := r.FormValue("body")
	if body == "" {
		rp.pages.Notice(w, "issue-comment", "Body is required")
		return
	}

	// TODO(boltless): normalize markdown body
	normalizedBody := body
	_, references := rp.mentionsResolver.Resolve(r.Context(), body)

	markdownBody := tangled.MarkupMarkdown{
		Text:     normalizedBody,
		Original: &body,
		Blobs:    nil,
	}

	// ingest CID of issue record on-demand.
	// TODO(boltless): appview should ingest CID of atproto records
	cid, err := func() (syntax.CID, error) {
		ident, err := rp.idResolver.ResolveIdent(r.Context(), issue.Did)
		if err != nil {
			return "", err
		}

		xrpcc := indigoxrpc.Client{Host: ident.PDSEndpoint()}
		out, err := comatproto.RepoGetRecord(r.Context(), &xrpcc, "", tangled.RepoIssueNSID, issue.Did, issue.Rkey)
		if err != nil {
			return "", err
		}
		if out.Cid == nil {
			return "", fmt.Errorf("record CID is empty")
		}

		cid, err := syntax.ParseCID(*out.Cid)
		if err != nil {
			return "", err
		}

		return cid, nil
	}()
	if err != nil {
		rp.logger.Error("failed to backfill subject PR record", "err", err)
		rp.pages.Notice(w, "issue-comment", "failed to backfill subject record")
		return
	}
	issueStrongRef := comatproto.RepoStrongRef{
		Uri: issue.AtUri().String(),
		Cid: cid.String(),
	}

	var replyTo *comatproto.RepoStrongRef
	replyToUriRaw := r.FormValue("reply-to-uri")
	replyToCidRaw := r.FormValue("reply-to-cid")
	if replyToUriRaw != "" && replyToCidRaw != "" {
		uri, err := syntax.ParseATURI(replyToUriRaw)
		if err != nil {
			rp.pages.Notice(w, "issue-comment", "reply-to-uri should be valid AT-URI")
			return
		}
		cid, err := syntax.ParseCID(replyToCidRaw)
		if err != nil {
			rp.pages.Notice(w, "issue-comment", "reply-to-cid should be valid CID")
			return
		}
		replyTo = &comatproto.RepoStrongRef{
			Uri: uri.String(),
			Cid: cid.String(),
		}
	}

	mentions, references := rp.mentionsResolver.Resolve(r.Context(), body)

	comment := models.Comment{
		Did:        syntax.DID(user.Did),
		Collection: tangled.FeedCommentNSID,
		Rkey:       syntax.RecordKey(tid.TID()),

		Subject: issueStrongRef,
		Body:    markdownBody,
		Created: time.Now(),
		ReplyTo: replyTo,
	}
	if err = comment.Validate(); err != nil {
		l.Error("failed to validate comment", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment.")
		return
	}

	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment.")
		return
	}

	// create a record first
	out, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: comment.Collection.String(),
		Repo:       comment.Did.String(),
		Rkey:       comment.Rkey.String(),
		Record:     &lexutil.LexiconTypeDecoder{Val: comment.AsRecord()},
	})
	if err != nil {
		l.Error("failed to create comment", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment.")
		return
	}

	comment.Cid = syntax.CID(out.Cid)

	tx, err := rp.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment, try again later.")
		return
	}
	defer tx.Rollback()

	err = db.PutComment(tx, &comment, references)
	if err != nil {
		l.Error("failed to create comment", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment.")
		return
	}

	err = tx.Commit()
	if err != nil {
		l.Error("failed to commit transaction", "err", err)
		rp.pages.Notice(w, "issue-comment", "Failed to create comment, try again later.")
		return
	}

	rp.notifier.NewComment(r.Context(), &comment, mentions)

	ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
	rp.pages.HxLocation(w, fmt.Sprintf("/%s/issues/%d#comment-%d", ownerSlashRepo, issue.IssueId, comment.Id))
}

func (rp *Issues) IssueComment(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "IssueComment")
	user := rp.oauth.GetMultiAccountUser(r)

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	commentId := chi.URLParam(r, "commentId")
	comments, err := db.GetComments(
		rp.db,
		orm.FilterEq("id", commentId),
	)
	if err != nil {
		l.Error("failed to fetch comment", "id", commentId)
		http.Error(w, "failed to fetch comment id", http.StatusBadRequest)
		return
	}
	if len(comments) != 1 {
		l.Error("incorrect number of comments returned", "id", commentId, "len(comments)", len(comments))
		http.Error(w, "invalid comment id", http.StatusBadRequest)
		return
	}
	comment := comments[0]

	rp.pages.IssueCommentBodyFragment(w, pages.IssueCommentBodyParams{
		LoggedInUser: user,
		RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
		Issue:        issue,
		Comment:      &comment,
	})
}

func (rp *Issues) EditIssueComment(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "EditIssueComment")
	user := rp.oauth.GetMultiAccountUser(r)

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	commentId := chi.URLParam(r, "commentId")
	comments, err := db.GetComments(
		rp.db,
		orm.FilterEq("id", commentId),
	)
	if err != nil {
		l.Error("failed to fetch comment", "id", commentId)
		http.Error(w, "failed to fetch comment id", http.StatusBadRequest)
		return
	}
	if len(comments) != 1 {
		l.Error("incorrect number of comments returned", "id", commentId, "len(comments)", len(comments))
		http.Error(w, "invalid comment id", http.StatusBadRequest)
		return
	}
	comment := comments[0]

	if comment.Did.String() != user.Did {
		l.Error("unauthorized comment edit", "expectedDid", comment.Did, "gotDid", user.Did)
		http.Error(w, "you are not the author of this comment", http.StatusUnauthorized)
		return
	}

	switch r.Method {
	case http.MethodGet:
		rp.pages.EditIssueCommentFragment(w, pages.EditIssueCommentParams{
			LoggedInUser: user,
			RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
			Issue:        issue,
			Comment:      &comment,
		})
	case http.MethodPost:
		// extract form value
		body := r.FormValue("body")
		if body == "" {
			rp.pages.Notice(w, "issue-comment", "Body is required")
			return
		}

		// TODO(boltless): normalize markdown body
		normalizedBody := body
		_, references := rp.mentionsResolver.Resolve(r.Context(), body)

		now := time.Now()
		newComment := comment
		newComment.Body = tangled.MarkupMarkdown{
			Text:     normalizedBody,
			Original: &body,
			Blobs:    nil,
		}
		newComment.Edited = &now

		client, err := rp.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			rp.pages.Notice(w, "issue-comment", "Failed to create comment.")
			return
		}

		// update a record first
		exCid := comment.Cid.String()
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: newComment.Collection.String(),
			Repo:       newComment.Did.String(),
			Rkey:       newComment.Rkey.String(),
			SwapRecord: &exCid,
			Record: &lexutil.LexiconTypeDecoder{
				Val: newComment.AsRecord(),
			},
		})
		if err != nil {
			l.Error("failed to update comment", "err", err)
			rp.pages.Notice(w, "issue-comment", "Failed to update comment, try again later.")
			return
		}

		newComment.Cid = syntax.CID(resp.Cid)

		tx, err := rp.db.Begin()
		if err != nil {
			l.Error("failed to start transaction", "err", err)
			rp.pages.Notice(w, "repo-notice", "Failed to update description, try again later.")
			return
		}
		defer tx.Rollback()

		err = db.PutComment(tx, &newComment, references)
		if err != nil {
			l.Error("failed to perform update-description query", "err", err)
			rp.pages.Notice(w, "repo-notice", "Failed to update description, try again later.")
			return
		}
		err = tx.Commit()
		if err != nil {
			l.Error("failed to commit transaction", "err", err)
			rp.pages.Notice(w, "issue-comment", "Failed to update comment, try again later.")
			return
		}

		// return new comment body with htmx
		rp.pages.IssueCommentBodyFragment(w, pages.IssueCommentBodyParams{
			LoggedInUser: user,
			RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
			Issue:        issue,
			Comment:      &newComment,
		})
	}
}

func (rp *Issues) ReplyIssueCommentPlaceholder(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "ReplyIssueCommentPlaceholder")
	user := rp.oauth.GetMultiAccountUser(r)

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	commentId := chi.URLParam(r, "commentId")
	comments, err := db.GetComments(
		rp.db,
		orm.FilterEq("id", commentId),
	)
	if err != nil {
		l.Error("failed to fetch comment", "id", commentId)
		http.Error(w, "failed to fetch comment id", http.StatusBadRequest)
		return
	}
	if len(comments) != 1 {
		l.Error("incorrect number of comments returned", "id", commentId, "len(comments)", len(comments))
		http.Error(w, "invalid comment id", http.StatusBadRequest)
		return
	}
	comment := comments[0]

	rp.pages.ReplyIssueCommentPlaceholderFragment(w, pages.ReplyIssueCommentPlaceholderParams{
		LoggedInUser: user,
		RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
		Issue:        issue,
		Comment:      &comment,
	})
}

func (rp *Issues) ReplyIssueComment(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "ReplyIssueComment")
	user := rp.oauth.GetMultiAccountUser(r)

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	commentId := chi.URLParam(r, "commentId")
	comments, err := db.GetComments(
		rp.db,
		orm.FilterEq("id", commentId),
	)
	if err != nil {
		l.Error("failed to fetch comment", "id", commentId)
		http.Error(w, "failed to fetch comment id", http.StatusBadRequest)
		return
	}
	if len(comments) != 1 {
		l.Error("incorrect number of comments returned", "id", commentId, "len(comments)", len(comments))
		http.Error(w, "invalid comment id", http.StatusBadRequest)
		return
	}
	comment := comments[0]

	rp.pages.ReplyIssueCommentFragment(w, pages.ReplyIssueCommentParams{
		LoggedInUser: user,
		RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
		Issue:        issue,
		Comment:      &comment,
	})
}

func (rp *Issues) DeleteIssueComment(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "DeleteIssueComment")
	user := rp.oauth.GetMultiAccountUser(r)

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		l.Error("failed to get issue")
		rp.pages.Error404(w)
		return
	}

	commentId := chi.URLParam(r, "commentId")
	comments, err := db.GetComments(
		rp.db,
		orm.FilterEq("id", commentId),
	)
	if err != nil {
		l.Error("failed to fetch comment", "id", commentId)
		http.Error(w, "failed to fetch comment id", http.StatusBadRequest)
		return
	}
	if len(comments) != 1 {
		l.Error("incorrect number of comments returned", "id", commentId, "len(comments)", len(comments))
		http.Error(w, "invalid comment id", http.StatusBadRequest)
		return
	}
	comment := comments[0]

	if comment.Did.String() != user.Did {
		l.Error("unauthorized action", "expectedDid", comment.Did, "gotDid", user.Did)
		http.Error(w, "you are not the author of this comment", http.StatusUnauthorized)
		return
	}

	if comment.Deleted != nil {
		http.Error(w, "comment already deleted", http.StatusBadRequest)
		return
	}

	// optimistic deletion
	deleted := time.Now()
	err = db.DeleteComments(rp.db, orm.FilterEq("id", comment.Id))
	if err != nil {
		l.Error("failed to delete comment", "err", err)
		rp.pages.Notice(w, fmt.Sprintf("comment-%s-status", commentId), "failed to delete comment")
		return
	}

	// delete from pds
	if comment.Rkey != "" {
		client, err := rp.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			rp.pages.Notice(w, "issue-comment", "Failed to delete comment.")
			return
		}
		_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: comment.Collection.String(),
			Repo:       comment.Did.String(),
			Rkey:       comment.Rkey.String(),
		})
		if err != nil {
			l.Error("failed to delete from PDS", "err", err)
		}
	}

	// optimistic update for htmx
	comment.Body = tangled.MarkupMarkdown{}
	comment.Deleted = &deleted

	// htmx fragment of comment after deletion
	rp.pages.IssueCommentBodyFragment(w, pages.IssueCommentBodyParams{
		LoggedInUser: user,
		RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
		Issue:        issue,
		Comment:      &comment,
	})
}

func (rp *Issues) RepoIssues(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RepoIssues")

	params := r.URL.Query()
	page := pagination.FromContext(r.Context())

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	query := searchquery.Parse(params.Get("q"))

	var isOpen *bool
	if urlState := params.Get("state"); urlState != "" {
		switch urlState {
		case "open":
			isOpen = ptrBool(true)
		case "closed":
			isOpen = ptrBool(false)
		}
		query.Set("state", urlState)
	} else if queryState := query.Get("state"); queryState != nil {
		switch *queryState {
		case "open":
			isOpen = ptrBool(true)
		case "closed":
			isOpen = ptrBool(false)
		}
	} else if _, hasQ := params["q"]; !hasQ {
		// no q param at all -- default to open
		isOpen = ptrBool(true)
		query.Set("state", "open")
	}

	resolve := func(ctx context.Context, ident string) (string, error) {
		id, err := rp.idResolver.ResolveIdent(ctx, ident)
		if err != nil {
			return "", err
		}
		return id.DID.String(), nil
	}

	authorDid, negatedAuthorDids := searchquery.ResolveAuthor(r.Context(), query, resolve, l)

	labels := query.GetAll("label")
	negatedLabels := query.GetAllNegated("label")
	labelValues := query.GetDynamicTags()
	negatedLabelValues := query.GetNegatedDynamicTags()

	// resolve DID-format label values: if a dynamic tag's label
	// definition has format "did", resolve the handle to a DID
	if len(labelValues) > 0 || len(negatedLabelValues) > 0 {
		labelDefs, err := db.GetLabelDefinitions(
			rp.db,
			orm.FilterIn("at_uri", f.Labels),
			orm.FilterContains("scope", tangled.RepoIssueNSID),
		)
		if err == nil {
			didLabels := make(map[string]bool)
			for _, def := range labelDefs {
				if def.ValueType.Format == models.ValueTypeFormatDid {
					didLabels[def.Name] = true
				}
			}
			labelValues = searchquery.ResolveDIDLabelValues(r.Context(), labelValues, didLabels, resolve, l)
			negatedLabelValues = searchquery.ResolveDIDLabelValues(r.Context(), negatedLabelValues, didLabels, resolve, l)
		} else {
			l.Debug("failed to fetch label definitions for DID resolution", "err", err)
		}
	}

	tf := searchquery.ExtractTextFilters(query)

	searchOpts := models.IssueSearchOptions{
		Keywords:           tf.Keywords,
		Phrases:            tf.Phrases,
		RepoDid:            f.RepoDid,
		IsOpen:             isOpen,
		AuthorDid:          authorDid,
		Labels:             labels,
		LabelValues:        labelValues,
		NegatedKeywords:    tf.NegatedKeywords,
		NegatedPhrases:     tf.NegatedPhrases,
		NegatedLabels:      negatedLabels,
		NegatedLabelValues: negatedLabelValues,
		NegatedAuthorDids:  negatedAuthorDids,
		Page:               page,
	}

	totalIssues := 0
	if isOpen == nil {
		totalIssues = f.RepoStats.IssueCount.Open + f.RepoStats.IssueCount.Closed
	} else if *isOpen {
		totalIssues = f.RepoStats.IssueCount.Open
	} else {
		totalIssues = f.RepoStats.IssueCount.Closed
	}

	repoInfo := rp.repoResolver.GetRepoInfo(r, user)

	var issues []models.Issue

	if searchOpts.HasSearchFilters() {
		res, err := rp.indexer.Search(r.Context(), searchOpts)
		if err != nil {
			l.Error("failed to search for issues", "err", err)
			return
		}
		l.Debug("searched issues with indexer", "count", len(res.Hits))
		totalIssues = int(res.Total)

		// update tab counts to reflect filtered results
		countOpts := searchOpts
		countOpts.Page = pagination.Page{Limit: 1}
		countOpts.IsOpen = ptrBool(true)
		if openRes, err := rp.indexer.Search(r.Context(), countOpts); err == nil {
			repoInfo.Stats.IssueCount.Open = int(openRes.Total)
		}
		countOpts.IsOpen = ptrBool(false)
		if closedRes, err := rp.indexer.Search(r.Context(), countOpts); err == nil {
			repoInfo.Stats.IssueCount.Closed = int(closedRes.Total)
		}

		if len(res.Hits) > 0 {
			issues, err = db.GetIssues(
				rp.db,
				orm.FilterIn("id", res.Hits),
			)
			if err != nil {
				l.Error("failed to get issues", "err", err)
				rp.pages.Notice(w, "issues", "Failed to load issues. Try again later.")
				return
			}
		}
	} else {
		filters := []orm.Filter{
			orm.FilterEq("repo_did", f.RepoDid),
		}
		if isOpen != nil {
			openInt := 0
			if *isOpen {
				openInt = 1
			}
			filters = append(filters, orm.FilterEq("open", openInt))
		}
		issues, err = db.GetIssuesPaginated(
			rp.db,
			page,
			filters...,
		)
		if err != nil {
			l.Error("failed to get issues", "err", err)
			rp.pages.Notice(w, "issues", "Failed to load issues. Try again later.")
			return
		}
	}

	labelDefs, err := db.GetLabelDefinitions(
		rp.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", tangled.RepoIssueNSID),
	)
	if err != nil {
		l.Error("failed to fetch labels", "err", err)
		rp.pages.Error503(w)
		return
	}

	defs := make(map[string]*models.LabelDefinition)
	for _, l := range labelDefs {
		defs[l.AtUri().String()] = &l
	}

	filterState := ""
	if isOpen != nil {
		if *isOpen {
			filterState = "open"
		} else {
			filterState = "closed"
		}
	}

	vouchRelationships := make(map[syntax.DID]*models.VouchRelationship)
	if user != nil {
		dids := make([]syntax.DID, len(issues))
		for i, u := range issues {
			dids[i] = syntax.DID(u.Did)
		}
		vouchRelationships, err = db.GetVouchRelationshipsBatch(rp.db, syntax.DID(user.Did), dids)
		if err != nil {
			l.Error("failed to fetch vouch relationships", "err", err)
		}
	}
	baseFilterParts := make([]string, 0, len(query.Items()))
	for _, item := range query.Items() {
		if item.Kind == searchquery.KindTagValue {
			if item.Key == "label" || !searchquery.KnownTags[item.Key] {
				continue
			}
		}
		baseFilterParts = append(baseFilterParts, item.Raw)
	}
	baseFilterQuery := strings.Join(baseFilterParts, " ")
	rp.pages.RepoIssues(w, pages.RepoIssuesParams{
		LoggedInUser:       rp.oauth.GetMultiAccountUser(r),
		RepoInfo:           repoInfo,
		Issues:             issues,
		IssueCount:         totalIssues,
		LabelDefs:          defs,
		FilterState:        filterState,
		FilterQuery:        query.String(),
		BaseFilterQuery:    baseFilterQuery,
		Page:               page,
		VouchRelationships: vouchRelationships,
	})
}

func ptrBool(b bool) *bool { return &b }

func (rp *Issues) NewIssue(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "NewIssue")
	user := rp.oauth.GetMultiAccountUser(r)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	switch r.Method {
	case http.MethodGet:
		rp.pages.RepoNewIssue(w, pages.RepoNewIssueParams{
			LoggedInUser: user,
			RepoInfo:     rp.repoResolver.GetRepoInfo(r, user),
		})
	case http.MethodPost:
		body := r.FormValue("body")
		mentions, references := rp.mentionsResolver.Resolve(r.Context(), body)

		issue := &models.Issue{
			RepoDid:    syntax.DID(f.RepoDid),
			Rkey:       tid.TID(),
			Title:      r.FormValue("title"),
			Body:       body,
			Open:       true,
			Did:        user.Did,
			Created:    time.Now(),
			Mentions:   mentions,
			References: references,
			Repo:       f,
		}

		if err := rp.validator.ValidateIssue(issue); err != nil {
			l.Error("validation error", "err", err)
			rp.pages.Notice(w, "issues", fmt.Sprintf("Failed to create issue: %s", err))
			return
		}

		record := issue.AsRecord()

		// create an atproto record
		client, err := rp.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			rp.pages.Notice(w, "issues", "Failed to create issue.")
			return
		}
		resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoIssueNSID,
			Repo:       user.Did,
			Rkey:       issue.Rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &record,
			},
		})
		if err != nil {
			l.Error("failed to create issue", "err", err)
			rp.pages.Notice(w, "issues", "Failed to create issue.")
			return
		}
		atUri := resp.Uri

		tx, err := rp.db.BeginTx(r.Context(), nil)
		if err != nil {
			rp.pages.Notice(w, "issues", "Failed to create issue, try again later")
			return
		}
		rollback := func() {
			err1 := tx.Rollback()
			err2 := rollbackRecord(context.Background(), atUri, client)

			if errors.Is(err1, sql.ErrTxDone) {
				err1 = nil
			}

			if err := errors.Join(err1, err2); err != nil {
				l.Error("failed to rollback txn", "err", err)
			}
		}
		defer rollback()

		err = db.PutIssue(tx, issue)
		if err != nil {
			l.Error("failed to create issue", "err", err)
			rp.pages.Notice(w, "issues", "Failed to create issue.")
			return
		}

		if err = tx.Commit(); err != nil {
			l.Error("failed to create issue", "err", err)
			rp.pages.Notice(w, "issues", "Failed to create issue.")
			return
		}

		// everything is successful, do not rollback the atproto record
		atUri = ""

		rp.notifier.NewIssue(r.Context(), issue, mentions)

		ownerSlashRepo := reporesolver.GetBaseRepoPath(r, f)
		rp.pages.HxLocation(w, fmt.Sprintf("/%s/issues/%d", ownerSlashRepo, issue.IssueId))
		return
	}
}

// this is used to rollback changes made to the PDS
//
// it is a no-op if the provided ATURI is empty
func rollbackRecord(ctx context.Context, aturi string, client *atclient.APIClient) error {
	if aturi == "" {
		return nil
	}

	parsed := syntax.ATURI(aturi)

	collection := parsed.Collection().String()
	repo := parsed.Authority().String()
	rkey := parsed.RecordKey().String()

	_, err := comatproto.RepoDeleteRecord(ctx, client, &comatproto.RepoDeleteRecord_Input{
		Collection: collection,
		Repo:       repo,
		Rkey:       rkey,
	})
	return err
}
