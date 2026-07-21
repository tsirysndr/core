package xrpc

import (
	"encoding/json"
	"net/http"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	xrpcerr "tangled.org/core/xrpc/errors"
)

func (x *Xrpc) FocusBegin(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "FocusBegin")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	if err := db.BeginFocus(x.DB, did); err != nil {
		l.Error("failed to begin focus", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	item, err := db.GetNextFocusItem(x.DB, did)
	if err != nil {
		l.Error("failed to get first focus item", "err", err)
		_ = db.EndFocus(x.DB, did)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	if item == nil {
		_ = db.EndFocus(x.DB, did)
		x.writeJSON(w, &tangled.TempFocusBeginSession_Output{})
		return
	}

	out := &tangled.TempFocusBeginSession_Output{NotificationId: &item.ID}
	setFocusSubject(item, &out.RepoDid, &out.IssueAt, &out.PullAt)

	x.writeJSON(w, out)
}

func (x *Xrpc) FocusNext(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "FocusNext")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	var input tangled.TempFocusNextItem_Input
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		writeError(w, errBadRequestBody, http.StatusBadRequest)
		return
	}

	if err := db.MarkNotificationRead(x.DB, input.CurrentId, did); err != nil {
		l.Warn("failed to mark notification read", "id", input.CurrentId, "err", err)
	}

	item, err := db.GetNextFocusItem(x.DB, did)
	if err != nil {
		l.Error("failed to get next focus item", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	if item == nil {
		_ = db.EndFocus(x.DB, did)
		x.writeJSON(w, &tangled.TempFocusNextItem_Output{})
		return
	}

	out := &tangled.TempFocusNextItem_Output{NotificationId: &item.ID}
	setFocusSubject(item, &out.RepoDid, &out.IssueAt, &out.PullAt)

	x.writeJSON(w, out)
}

func (x *Xrpc) FocusEnd(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "FocusEnd")

	did, ok := actorDid(r)
	if !ok {
		writeError(w, xrpcerr.MissingActorDidError, http.StatusForbidden)
		return
	}

	if err := db.EndFocus(x.DB, did); err != nil {
		l.Error("failed to end focus", "err", err)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	w.WriteHeader(http.StatusOK)
}

// setFocusSubject fills the repo did and (issue|pull) at-uri of a focus item so
// the client can navigate to it.
func setFocusSubject(n *models.NotificationWithEntity, repoDid, issueAt, pullAt **string) {
	if n.Repo != nil {
		s := n.Repo.RepoDid
		*repoDid = &s
	}
	if n.Issue != nil {
		s := n.Issue.AtUri().String()
		*issueAt = &s
	}
	if n.Pull != nil {
		s := n.Pull.AtUri().String()
		*pullAt = &s
	}
}
