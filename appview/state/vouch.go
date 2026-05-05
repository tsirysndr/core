package state

import (
	"net/http"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"github.com/ipfs/go-cid"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/log"
)

func (s *State) Vouch(w http.ResponseWriter, r *http.Request) {
	l := log.SubLogger(s.logger, "vouch")
	l = s.logger.With("handler", "Vouch")
	currentUser := s.oauth.GetMultiAccountUser(r)

	var subject string

	subject = r.FormValue("subject")

	if subject == "" {
		l.Warn("invalid form: missing subject")
		s.pages.Notice(w, "error", "Missing subject user.")
		return
	}

	subjectIdent, err := s.idResolver.ResolveIdent(r.Context(), subject)
	if err != nil {
		l.Error("failed to vouch, invalid did", "subject", subject, "err", err)
		s.pages.Notice(w, "error", "Could not find that user.")
		return
	}

	if currentUser.Did == subjectIdent.DID.String() {
		l.Warn("cant vouch or denounce yourself")
		s.pages.Notice(w, "error", "You cannot vouch for yourself.")
		return
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		s.pages.Notice(w, "error", "Authentication required.")
		return
	}

	if err := r.ParseForm(); err != nil {
		l.Warn("failed to parse form", "err", err)
		s.pages.Notice(w, "error", "Invalid form data.")
		return
	}

	subjectDid := subjectIdent.DID.String()

	// handle "none" by deleting any existing vouch
	if r.FormValue("kind") == "none" {
		_, err := db.GetVouch(s.db, currentUser.Did, subjectDid)
		if err != nil {
			l.Info("no existing vouch to delete")
			s.pages.HxRefresh(w)
			return
		}

		_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: tangled.GraphVouchNSID,
			Repo:       currentUser.Did,
			Rkey:       subjectDid,
		})

		if err != nil {
			l.Error("failed to delete vouch record", "err", err)
			s.pages.Notice(w, "error", "Failed to delete vouch record.")
			return
		}

		err = db.DeleteVouch(s.db, currentUser.Did, subjectDid)
		if err != nil {
			l.Warn("failed to delete vouch from DB", "err", err)
		}

		l.Info("deleted vouch record")
		s.pages.HxRefresh(w)
		return
	}

	kind, err := models.ParseVouchKind(r.FormValue("kind"))
	if err != nil {
		l.Warn("failed to parse vouch kind", "err", err)
		s.pages.Notice(w, "error", "Invalid action type.")
		return
	}

	reason := r.FormValue("reason")
	createdAt := time.Now().Format(time.RFC3339)

	var reasonPtr *string
	if reason != "" {
		reasonPtr = &reason
	}

	var evidences []string
	for _, raw := range r.Form["evidences"] {
		if _, err := syntax.ParseATURI(raw); err != nil {
			l.Warn("invalid evidence AT-URI, skipping", "uri", raw, "err", err)
			continue
		}
		evidences = append(evidences, raw)
	}

	var swapCid *string
	existingVouch, err := db.GetVouch(s.db, currentUser.Did, subjectDid)
	if err == nil {
		cidStr := existingVouch.Cid.String()
		swapCid = &cidStr
	}

	resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.GraphVouchNSID,
		Repo:       currentUser.Did,
		Rkey:       subjectDid,
		SwapRecord: swapCid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &tangled.GraphVouch{
				Kind:      string(kind),
				Reason:    reasonPtr,
				CreatedAt: createdAt,
				Evidences: evidences,
			}},
	})
	if err != nil {
		l.Error("failed to create atproto record", "err", err)
		s.pages.Notice(w, "error", "Failed to create vouch record.")
		return
	}

	l.Info("created atproto record", "uri", resp.Uri, "kind", kind)

	newCid, err := cid.Parse(resp.Cid)
	if err != nil {
		l.Error("failed to parse returned cid", "err", err)
		s.pages.Notice(w, "error", "Failed to save vouch.")
		return
	}

	vouch := &models.Vouch{
		Did:        syntax.DID(currentUser.Did),
		SubjectDid: subjectIdent.DID,
		Cid:        newCid,
		Kind:       kind,
		Reason:     reasonPtr,
		Evidences:  evidences,
	}

	tx, err := s.db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		s.pages.Notice(w, "error", "Failed to save vouch.")
		return
	}
	defer tx.Rollback()

	err = db.AddVouch(tx, vouch)
	if err != nil {
		l.Error("failed to add vouch to db", "err", err)
		s.pages.Notice(w, "error", "Failed to save vouch.")
		return
	}

	if err = tx.Commit(); err != nil {
		l.Error("failed to commit vouch transaction", "err", err)
		s.pages.Notice(w, "error", "Failed to save vouch.")
		return
	}

	s.pages.HxRefresh(w)
}
