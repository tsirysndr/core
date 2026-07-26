package pulls

import (
	"net/http"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	lexutil "github.com/bluesky-social/indigo/lex/util"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
)

func (s *Pulls) EditPull(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "EditPull")
	user := s.oauth.GetMultiAccountUser(r)
	ctx := r.Context()

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Error404(w)
		return
	}

	switch r.Method {
	case http.MethodGet:
		s.pages.EditPullFragment(w, pages.EditPullParams{
			LoggedInUser: user,
			RepoInfo:     s.repoResolver.GetRepoInfo(r, user),
			Pull:         pull,
		})
	case http.MethodPost:
		noticeId := "pulls"
		newPull := *pull
		newPull.Title = r.FormValue("title")
		newPull.Body = r.FormValue("body")
		newPull.Mentions, newPull.References = s.mentionsResolver.Resolve(ctx, newPull.Body)

		// edit an atproto record
		client, err := s.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to get authorized client", "err", err)
			s.pages.Notice(w, noticeId, "Failed to edit pull.")
			return
		}

		ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoPullNSID, user.Did, newPull.Rkey)
		if err != nil {
			l.Error("failed to get record", "err", err)
			s.pages.Notice(w, noticeId, "Failed to edit pull, no record found on PDS.")
			return
		}

		// merge existing pins with new uploads, dropping any removed from body
		var existingBlobs []*lexutil.LexBlob
		if ex.Value != nil {
			if prev, ok := ex.Value.Val.(*tangled.RepoPull); ok {
				existingBlobs = prev.Blobs
			}
		}
		newPull.Blobs = models.MergeBlobs(existingBlobs, r.PostForm["blobs"], newPull.Body)

		newRecord := newPull.AsRecord()
		_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoPullNSID,
			Repo:       user.Did,
			Rkey:       newPull.Rkey,
			SwapRecord: ex.Cid,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &newRecord,
			},
		})
		if err != nil {
			l.Error("failed to edit record on PDS", "err", err)
			s.pages.Notice(w, noticeId, "Failed to edit pull on PDS.")
			return
		}

		tx, err := s.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Error("failed to start tx", "err", err)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}
		defer tx.Rollback()

		err = db.PutPull(tx, &newPull)
		if err != nil {
			l.Error("failed to create pull request in database", "err", err)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}

		if err = tx.Commit(); err != nil {
			l.Error("failed to commit transaction for pull request", "err", err)
			s.pages.Notice(w, "pull", "Failed to create pull request. Try again later.")
			return
		}

		s.pages.HxRefresh(w)
	}
}
