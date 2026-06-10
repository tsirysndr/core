package knots

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/middleware"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/serververify"
	"tangled.org/core/appview/xrpcclient"
	"tangled.org/core/consts"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/idresolver"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/tid"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	lexutil "github.com/bluesky-social/indigo/lex/util"
)

type Knots struct {
	Db         *db.DB
	OAuth      *oauth.OAuth
	Pages      *pages.Pages
	Config     *config.Config
	Enforcer   *rbac.Enforcer
	Acl        *knotacl.Service
	IdResolver *idresolver.Resolver
	Logger     *slog.Logger
	Knotstream *eventconsumer.Consumer
}

func (k *Knots) Router() http.Handler {
	r := chi.NewRouter()

	r.With(middleware.AuthMiddleware(k.OAuth)).Get("/", k.knots)
	r.With(middleware.AuthMiddleware(k.OAuth)).Post("/register", k.register)

	r.With(middleware.AuthMiddleware(k.OAuth)).Get("/{domain}", k.dashboard)
	r.With(middleware.AuthMiddleware(k.OAuth)).Delete("/{domain}", k.delete)

	r.With(middleware.AuthMiddleware(k.OAuth)).Post("/{domain}/retry", k.retry)
	r.With(middleware.AuthMiddleware(k.OAuth)).Post("/{domain}/add", k.addMember)
	r.With(middleware.AuthMiddleware(k.OAuth)).Post("/{domain}/remove", k.removeMember)

	return r
}

func (k *Knots) knots(w http.ResponseWriter, r *http.Request) {
	user := k.OAuth.GetMultiAccountUser(r)
	registrations, err := db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
	)
	if err != nil {
		k.Logger.Error("failed to fetch knot registrations", "err", err)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}

	knots := make([]pages.KnotListingParams, 0, len(registrations))
	for i := range registrations {
		registration := &registrations[i]
		count, err := db.CountRepos(k.Db, orm.FilterEq("knot", registration.Domain))
		if err != nil {
			k.Logger.Error("failed to count knot repos", "err", err, "domain", registration.Domain)
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		knots = append(knots, pages.KnotListingParams{
			Registration: registration,
			RepoCount:    int(count),
		})
	}

	k.Pages.Knots(w, pages.KnotsParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		Knots:      knots,
	})
}

func (k *Knots) dashboard(w http.ResponseWriter, r *http.Request) {
	l := k.Logger.With("handler", "dashboard")

	user := k.OAuth.GetMultiAccountUser(r)
	l = l.With("user", user.Did)

	domain := chi.URLParam(r, "domain")
	if domain == "" {
		return
	}
	l = l.With("domain", domain)

	registrations, err := db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
	)
	if err != nil {
		l.Error("failed to get registrations", "err", err)
		http.Error(w, "Not found", http.StatusNotFound)
		return
	}
	if len(registrations) != 1 {
		l.Error("got incorrect number of registrations", "got", len(registrations), "expected", 1)
		return
	}
	registration := registrations[0]

	members := k.Acl.KnotMembers(r.Context(), domain)

	repos, err := db.GetRepos(
		k.Db,
		orm.FilterEq("knot", domain),
	)
	if err != nil {
		l.Error("failed to get knot repos", "err", err)
		http.Error(w, "Not found", http.StatusInternalServerError)
		return
	}

	// organize repos by did
	repoMap := make(map[string][]models.Repo)
	for _, r := range repos {
		repoMap[r.Did] = append(repoMap[r.Did], r)
	}

	k.Pages.Knot(w, pages.KnotParams{
		BaseParams:   pages.BaseParamsFromContext(r.Context()),
		Registration: &registration,
		Members:      members,
		Repos:        repoMap,
		IsOwner:      true,
		RepoCount:    len(repos),
	})
}

func (k *Knots) register(w http.ResponseWriter, r *http.Request) {
	user := k.OAuth.GetMultiAccountUser(r)
	l := k.Logger.With("handler", "register")

	noticeId := "register-error"
	defaultErr := "Failed to register knot. Try again later."
	fail := func() {
		k.Pages.Notice(w, noticeId, defaultErr)
	}

	domain := r.FormValue("domain")
	// Strip protocol, trailing slashes, and whitespace
	// Rkey cannot contain slashes
	domain = strings.TrimSpace(domain)
	domain = strings.TrimPrefix(domain, "https://")
	domain = strings.TrimPrefix(domain, "http://")
	domain = strings.TrimSuffix(domain, "/")
	if domain == "" {
		k.Pages.Notice(w, noticeId, "Incomplete form.")
		return
	}
	l = l.With("domain", domain)
	l = l.With("user", user.Did)

	tx, err := k.Db.Begin()
	if err != nil {
		l.Error("failed to start transaction", "err", err)
		fail()
		return
	}
	defer tx.Rollback()

	if err := db.AddKnot(tx, domain, user.Did); err != nil {
		l.Error("failed to insert", "err", err)
		fail()
		return
	}

	client, err := k.OAuth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		fail()
		return
	}

	ex, _ := comatproto.RepoGetRecord(r.Context(), client, "", tangled.KnotNSID, user.Did, domain)
	var exCid *string
	if ex != nil {
		exCid = ex.Cid
	}

	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.KnotNSID,
		Repo:       user.Did,
		Rkey:       domain,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &tangled.Knot{
				CreatedAt: time.Now().Format(time.RFC3339),
			},
		},
		SwapRecord: exCid,
	})
	if err != nil {
		l.Error("failed to put record", "err", err)
		fail()
		return
	}

	if err := tx.Commit(); err != nil {
		l.Error("failed to commit transaction", "err", err)
		fail()
		return
	}

	go k.Knotstream.AddSource(r.Context(), eventconsumer.NewKnotSource(domain))

	k.Pages.HxRefresh(w)
}

func (k *Knots) delete(w http.ResponseWriter, r *http.Request) {
	user := k.OAuth.GetMultiAccountUser(r)
	l := k.Logger.With("handler", "delete")

	noticeId := "operation-error"
	defaultErr := "Failed to delete knot. Try again later."
	fail := func() {
		k.Pages.Notice(w, noticeId, defaultErr)
	}

	domain := chi.URLParam(r, "domain")
	if domain == "" {
		l.Error("empty domain")
		fail()
		return
	}

	// get record from db first
	registrations, err := db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
	)
	if err != nil {
		l.Error("failed to get registration", "err", err)
		fail()
		return
	}
	if len(registrations) != 1 {
		l.Error("got incorrect number of registrations", "got", len(registrations), "expected", 1)
		fail()
		return
	}
	registration := registrations[0]

	tx, err := k.Db.Begin()
	if err != nil {
		l.Error("failed to start txn", "err", err)
		fail()
		return
	}
	defer func() {
		tx.Rollback()
		k.Enforcer.E.LoadPolicy()
	}()

	err = db.DeleteKnot(
		tx,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
	)
	if err != nil {
		l.Error("failed to delete registration", "err", err)
		fail()
		return
	}

	err = db.RemoveReposByKnot(tx, domain)
	if err != nil {
		l.Error("failed to delete repos", "err", err)
		fail()
		return
	}

	// delete from enforcer if it was registered
	if registration.Registered != nil {
		err = k.Enforcer.RemoveKnot(domain)
		if err != nil {
			l.Error("failed to update ACL", "err", err)
			fail()
			return
		}
	}

	client, err := k.OAuth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		fail()
		return
	}

	_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
		Collection: tangled.KnotNSID,
		Repo:       user.Did,
		Rkey:       domain,
	})
	if err != nil {
		// non-fatal
		l.Error("failed to delete record", "err", err)
	}

	err = tx.Commit()
	if err != nil {
		l.Error("failed to delete knot", "err", err)
		fail()
		return
	}

	err = k.Enforcer.E.SavePolicy()
	if err != nil {
		l.Error("failed to update ACL", "err", err)
		k.Pages.HxRefresh(w)
		return
	}

	if registration.Registered != nil {
		remaining, rErr := db.GetRegistrations(k.Db,
			orm.FilterEq("domain", domain),
			orm.FilterIsNot("registered", "null"),
		)
		if rErr != nil {
			l.Warn("failed to check remaining registrations after delete", "err", rErr)
		} else if len(remaining) == 0 {
			go k.Knotstream.RemoveSource(eventconsumer.NewKnotSource(domain))
		}
	}

	shouldRedirect := r.Header.Get("shouldRedirect")
	if shouldRedirect == "true" {
		k.Pages.HxRedirect(w, "/knots")
		return
	}

	w.Write([]byte{})
}

func (k *Knots) retry(w http.ResponseWriter, r *http.Request) {
	user := k.OAuth.GetMultiAccountUser(r)
	l := k.Logger.With("handler", "retry")

	noticeId := "operation-error"
	defaultErr := "Failed to verify knot. Try again later."
	fail := func() {
		k.Pages.Notice(w, noticeId, defaultErr)
	}

	domain := chi.URLParam(r, "domain")
	if domain == "" {
		l.Error("empty domain")
		fail()
		return
	}
	l = l.With("domain", domain)
	l = l.With("user", user.Did)

	// get record from db first
	registrations, err := db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
	)
	if err != nil {
		l.Error("failed to get registration", "err", err)
		fail()
		return
	}
	if len(registrations) != 1 {
		l.Error("got incorrect number of registrations", "got", len(registrations), "expected", 1)
		fail()
		return
	}
	registration := registrations[0]

	// begin verification
	err = serververify.RunVerification(r.Context(), domain, user.Did, k.Config.Core.Dev)
	if err != nil {
		l.Error("verification failed", "err", err)

		if errors.Is(err, xrpcclient.ErrXrpcUnsupported) {
			k.Pages.Notice(w, noticeId, "Failed to verify knot, XRPC queries are unsupported on this knot, consider upgrading!")
			return
		}

		if e, ok := err.(*serververify.OwnerMismatch); ok {
			k.Pages.Notice(w, noticeId, e.Error())
			return
		}

		fail()
		return
	}

	err = serververify.MarkKnotVerified(k.Db, k.Enforcer, domain, user.Did)
	if err != nil {
		l.Error("failed to mark verified", "err", err)
		k.Pages.Notice(w, noticeId, err.Error())
		return
	}

	// if this knot requires upgrade, then emit a record too
	//
	// this is part of migrating from the old knot system to the new one
	if registration.NeedsUpgrade {
		// re-announce by registering under same rkey
		client, err := k.OAuth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to authorize client", "err", err)
			fail()
			return
		}

		ex, _ := comatproto.RepoGetRecord(r.Context(), client, "", tangled.KnotNSID, user.Did, domain)
		var exCid *string
		if ex != nil {
			exCid = ex.Cid
		}

		// ignore the error here
		_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.KnotNSID,
			Repo:       user.Did,
			Rkey:       domain,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &tangled.Knot{
					CreatedAt: time.Now().Format(time.RFC3339),
				},
			},
			SwapRecord: exCid,
		})
		if err != nil {
			l.Error("non-fatal: failed to reannouce knot", "err", err)
		}
	}

	// add this knot to knotstream
	go k.Knotstream.AddSource(
		r.Context(),
		eventconsumer.NewKnotSource(domain),
	)

	shouldRefresh := r.Header.Get("shouldRefresh")
	if shouldRefresh == "true" {
		k.Pages.HxRefresh(w)
		return
	}

	// Get updated registration to show
	registrations, err = db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
	)
	if err != nil {
		l.Error("failed to get registration", "err", err)
		fail()
		return
	}
	if len(registrations) != 1 {
		l.Error("got incorrect number of registrations", "got", len(registrations), "expected", 1)
		fail()
		return
	}
	updatedRegistration := registrations[0]

	count, err := db.CountRepos(k.Db, orm.FilterEq("knot", domain))
	if err != nil {
		l.Error("failed to count knot repos", "err", err)
		fail()
		return
	}

	w.Header().Set("HX-Reswap", "outerHTML")
	k.Pages.KnotListing(w, pages.KnotListingParams{
		Registration: &updatedRegistration,
		RepoCount:    int(count),
	})
}

func (k *Knots) addMember(w http.ResponseWriter, r *http.Request) {
	user := k.OAuth.GetMultiAccountUser(r)
	l := k.Logger.With("handler", "addMember")

	domain := chi.URLParam(r, "domain")
	if domain == "" {
		l.Error("empty domain")
		http.Error(w, "Not found", http.StatusNotFound)
		return
	}
	l = l.With("domain", domain)
	l = l.With("user", user.Did)

	registrations, err := db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
		orm.FilterIsNot("registered", "null"),
	)
	if err != nil {
		l.Error("failed to get registration", "err", err)
		return
	}
	if len(registrations) != 1 {
		l.Error("got incorrect number of registrations", "got", len(registrations), "expected", 1)
		return
	}
	registration := registrations[0]

	noticeId := fmt.Sprintf("add-member-error-%d", registration.Id)
	defaultErr := "Failed to add member. Try again later."
	fail := func() {
		k.Pages.Notice(w, noticeId, defaultErr)
	}

	member := r.FormValue("member")
	member = strings.TrimPrefix(member, "@")
	if member == "" {
		l.Error("empty member")
		k.Pages.Notice(w, noticeId, "Failed to add member, empty form.")
		return
	}
	l = l.With("member", member)

	memberId, err := k.IdResolver.ResolveIdent(r.Context(), member)
	if err != nil {
		l.Error("failed to resolve member identity to handle", "err", err)
		k.Pages.Notice(w, noticeId, "Failed to add member, identity resolution failed.")
		return
	}
	if memberId.Handle.IsInvalidHandle() {
		l.Error("failed to resolve member identity to handle")
		k.Pages.Notice(w, noticeId, "Failed to add member, identity resolution failed.")
		return
	}

	capStatus := knotcompat.KnotCapability(r.Context(), domain, k.Config.Core.Dev, consts.CapKnotACL)
	if capStatus == knotcompat.CapUnknown {
		l.Error("knot capability probe failed")
		k.Pages.Notice(w, noticeId, "Could not reach the knot to add the member. Try again later.")
		return
	}
	if capStatus == knotcompat.CapPresent {
		client, err := k.OAuth.ServiceClient(
			r,
			oauth.WithService(domain),
			oauth.WithLxm(tangled.KnotAddMemberNSID),
			oauth.WithDev(k.Config.Core.Dev),
		)
		if err != nil {
			l.Error("failed to create knot service client", "err", err)
			fail()
			return
		}

		err = tangled.KnotAddMember(r.Context(), client, &tangled.KnotAddMember_Input{
			Subject: memberId.DID.String(),
		})
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC knot.addMember", "xrpcerr", xrpcerr, "err", err)
			k.Pages.Notice(w, noticeId, xrpcerr.Error())
			return
		}

		k.Acl.InvalidateMembers(domain)

		k.Pages.HxRedirect(w, fmt.Sprintf("/settings/knots/%s", domain))
		return
	}

	client, err := k.OAuth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		fail()
		return
	}

	rkey := tid.TID()
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.KnotMemberNSID,
		Repo:       user.Did,
		Rkey:       rkey,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &tangled.KnotMember{
				CreatedAt: time.Now().Format(time.RFC3339),
				Domain:    domain,
				Subject:   memberId.DID.String(),
			},
		},
	})
	if err != nil {
		l.Error("failed to add record to PDS", "err", err)
		k.Pages.Notice(w, noticeId, "Failed to add record to PDS, try again later.")
		return
	}

	k.Pages.HxRedirect(w, fmt.Sprintf("/settings/knots/%s", domain))
}

func (k *Knots) removeMember(w http.ResponseWriter, r *http.Request) {
	user := k.OAuth.GetMultiAccountUser(r)
	l := k.Logger.With("handler", "removeMember")

	noticeId := "operation-error"
	defaultErr := "Failed to remove member. Try again later."
	fail := func() {
		k.Pages.Notice(w, noticeId, defaultErr)
	}

	domain := chi.URLParam(r, "domain")
	if domain == "" {
		l.Error("empty domain")
		fail()
		return
	}
	l = l.With("domain", domain)
	l = l.With("user", user.Did)

	registrations, err := db.GetRegistrations(
		k.Db,
		orm.FilterEq("did", user.Did),
		orm.FilterEq("domain", domain),
		orm.FilterIsNot("registered", "null"),
	)
	if err != nil {
		l.Error("failed to get registration", "err", err)
		return
	}
	if len(registrations) != 1 {
		l.Error("got incorrect number of registrations", "got", len(registrations), "expected", 1)
		return
	}

	member := r.FormValue("member")
	member = strings.TrimPrefix(member, "@")
	if member == "" {
		l.Error("empty member")
		k.Pages.Notice(w, noticeId, "Failed to remove member, empty form.")
		return
	}
	l = l.With("member", member)

	memberId, err := k.IdResolver.ResolveIdent(r.Context(), member)
	if err != nil {
		l.Error("failed to resolve member identity to handle", "err", err)
		k.Pages.Notice(w, noticeId, "Failed to remove member, identity resolution failed.")
		return
	}

	capStatus := knotcompat.KnotCapability(r.Context(), domain, k.Config.Core.Dev, consts.CapKnotACL)
	if capStatus == knotcompat.CapUnknown {
		l.Error("knot capability probe failed")
		k.Pages.Notice(w, noticeId, "Could not reach the knot to remove the member. Try again later.")
		return
	}
	if capStatus == knotcompat.CapPresent {
		client, err := k.OAuth.ServiceClient(
			r,
			oauth.WithService(domain),
			oauth.WithLxm(tangled.KnotRemoveMemberNSID),
			oauth.WithDev(k.Config.Core.Dev),
		)
		if err != nil {
			l.Error("failed to create knot service client", "err", err)
			fail()
			return
		}

		err = tangled.KnotRemoveMember(r.Context(), client, &tangled.KnotRemoveMember_Input{
			Subject: memberId.DID.String(),
		})
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC knot.removeMember", "xrpcerr", xrpcerr, "err", err)
			k.Pages.Notice(w, noticeId, xrpcerr.Error())
			return
		}

		k.Acl.InvalidateMembers(domain)

		k.Pages.HxRefresh(w)
		return
	}

	client, err := k.OAuth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to authorize client", "err", err)
		fail()
		return
	}

	rkey, err := lookupKnotMemberRkey(r.Context(), k.Db, client, user.Did, domain, memberId.DID.String())
	if err != nil {
		l.Warn("failed to look up member rkey", "err", err)
	}

	if rkey == "" {
		l.Error("no member record found to remove")
		fail()
		return
	}

	_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
		Collection: tangled.KnotMemberNSID,
		Repo:       user.Did,
		Rkey:       rkey,
	})
	if err != nil {
		l.Error("failed to delete record from PDS", "err", err)
		k.Pages.Notice(w, noticeId, "Failed to delete record from PDS, try again later.")
		return
	}

	k.Pages.HxRefresh(w)
}

func lookupKnotMemberRkey(ctx context.Context, d *db.DB, client *atclient.APIClient, ownerDid, domain, subject string) (string, error) {
	members, err := db.GetKnotMembers(
		d,
		orm.FilterEq("did", ownerDid),
		orm.FilterEq("domain", domain),
		orm.FilterEq("subject", subject),
	)
	if err != nil {
		return "", fmt.Errorf("db lookup: %w", err)
	}
	if len(members) >= 1 {
		return members[0].Rkey, nil
	}
	return findKnotMemberRkey(ctx, client, ownerDid, domain, subject, "")
}

func findKnotMemberRkey(ctx context.Context, client *atclient.APIClient, repo, domain, subject, cursor string) (string, error) {
	out, err := comatproto.RepoListRecords(ctx, client, tangled.KnotMemberNSID, cursor, 100, repo, false)
	if err != nil {
		return "", err
	}
	for _, rec := range out.Records {
		m, ok := rec.Value.Val.(*tangled.KnotMember)
		if !ok {
			continue
		}
		if m.Domain != domain || m.Subject != subject {
			continue
		}
		parts := strings.Split(rec.Uri, "/")
		return parts[len(parts)-1], nil
	}
	if out.Cursor == nil || *out.Cursor == "" || *out.Cursor == cursor {
		return "", nil
	}
	return findKnotMemberRkey(ctx, client, repo, domain, subject, *out.Cursor)
}
