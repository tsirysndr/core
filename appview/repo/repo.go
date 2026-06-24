package repo

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"net/url"
	"slices"
	"strings"
	"time"

	"tangled.org/core/appview/cloudflare"
	"tangled.org/core/appview/codesearch"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/knotcompat"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/notify"
	"tangled.org/core/appview/oauth"
	"tangled.org/core/appview/pages"
	"tangled.org/core/appview/pagination"
	"tangled.org/core/appview/reporesolver"
	"tangled.org/core/appview/sites"
	"tangled.org/core/appview/validator"
	xrpcclient "tangled.org/core/appview/xrpcclient"
	"tangled.org/core/consts"
	"tangled.org/core/eventconsumer"
	"tangled.org/core/idresolver"
	"tangled.org/core/ogre"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/tid"
	"tangled.org/core/xrpc/serviceauth"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"

	"github.com/go-chi/chi/v5"
)

type Repo struct {
	repoResolver  *reporesolver.RepoResolver
	idResolver    *idresolver.Resolver
	config        *config.Config
	oauth         *oauth.OAuth
	pages         *pages.Pages
	spindlestream *eventconsumer.Consumer
	db            *db.DB
	enforcer      *rbac.Enforcer
	acl           *knotacl.Service
	notifier      notify.Notifier
	logger        *slog.Logger
	serviceAuth   *serviceauth.ServiceAuth
	validator     *validator.Validator
	cfClient      *cloudflare.Client
	ogreClient    *ogre.Client
	codesearch    *codesearch.CodeSearch
}

func New(
	oauth *oauth.OAuth,
	repoResolver *reporesolver.RepoResolver,
	pages *pages.Pages,
	spindlestream *eventconsumer.Consumer,
	idResolver *idresolver.Resolver,
	db *db.DB,
	config *config.Config,
	notifier notify.Notifier,
	enforcer *rbac.Enforcer,
	acl *knotacl.Service,
	logger *slog.Logger,
	validator *validator.Validator,
	cfClient *cloudflare.Client,
	codesearch *codesearch.CodeSearch,
) *Repo {
	return &Repo{
		oauth:         oauth,
		repoResolver:  repoResolver,
		pages:         pages,
		idResolver:    idResolver,
		config:        config,
		spindlestream: spindlestream,
		db:            db,
		notifier:      notifier,
		enforcer:      enforcer,
		acl:           acl,
		logger:        logger,
		validator:     validator,
		cfClient:      cfClient,
		ogreClient:    ogre.NewClient(config.Ogre.Host),
		codesearch:    codesearch,
	}
}

// modify the spindle configured for this repo
func (rp *Repo) EditSpindle(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "EditSpindle")
	l = l.With("did", user.Did)

	errorId := "operation-error"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		fail("Failed to resolve repo. Try again later", err)
		return
	}

	newSpindle := r.FormValue("spindle")
	removingSpindle := newSpindle == "[[none]]" // see pages/templates/repo/settings/pipelines.html for more info on why we use this value
	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail("Failed to authorize. Try again later.", err)
		return
	}

	if !removingSpindle {
		// ensure that this is a valid spindle for this user
		validSpindles, err := rp.enforcer.GetSpindlesForUser(user.Did)
		if err != nil {
			fail("Failed to find spindles. Try again later.", err)
			return
		}

		if !slices.Contains(validSpindles, newSpindle) {
			fail("Failed to configure spindle.", fmt.Errorf("%s is not a valid spindle: %q", newSpindle, validSpindles))
			return
		}
	}

	newRepo := *f
	newRepo.Spindle = newSpindle
	record := newRepo.AsRecord()

	spindlePtr := &newSpindle
	if removingSpindle {
		spindlePtr = nil
		newRepo.Spindle = ""
	}

	// optimistic update
	err = db.UpdateSpindle(rp.db, newRepo.RepoDid, spindlePtr)
	if err != nil {
		fail("Failed to update spindle. Try again later.", err)
		return
	}

	ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoNSID, newRepo.Did, newRepo.Rkey)
	if err != nil {
		fail("Failed to update spindle, no record found on PDS.", err)
		return
	}
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoNSID,
		Repo:       newRepo.Did,
		Rkey:       newRepo.Rkey,
		SwapRecord: ex.Cid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &record,
		},
	})

	if err != nil {
		fail("Failed to update spindle, unable to save to PDS.", err)
		return
	}

	oldSpindle := f.Spindle
	if oldSpindle != "" && oldSpindle != newSpindle {
		remaining, qErr := db.GetRepos(rp.db, orm.FilterEq("spindle", oldSpindle))
		if qErr != nil {
			l.Warn("failed to count repos using old spindle", "err", qErr)
		} else if len(remaining) == 0 {
			rp.spindlestream.RemoveSource(eventconsumer.NewSpindleSource(oldSpindle))
		}
	}

	if !removingSpindle {
		rp.spindlestream.AddSource(
			context.Background(),
			eventconsumer.NewSpindleSource(newSpindle),
		)
	}

	rp.pages.HxRefresh(w)
}

func (rp *Repo) AddLabelDef(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "AddLabel")
	l = l.With("did", user.Did)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	errorId := "add-label-error"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	// get form values for label definition
	name := r.FormValue("name")
	concreteType := r.FormValue("valueType")
	valueFormat := r.FormValue("valueFormat")
	enumValues := r.FormValue("enumValues")
	scope := r.Form["scope"]
	color := r.FormValue("color")
	multiple := r.FormValue("multiple") == "true"

	var variants []string
	for part := range strings.SplitSeq(enumValues, ",") {
		if part = strings.TrimSpace(part); part != "" {
			variants = append(variants, part)
		}
	}

	if concreteType == "" {
		concreteType = "null"
	}

	format := models.ValueTypeFormatAny
	if valueFormat == "did" {
		format = models.ValueTypeFormatDid
	}

	valueType := models.ValueType{
		Type:   models.ConcreteType(concreteType),
		Format: format,
		Enum:   variants,
	}

	label := models.LabelDefinition{
		Did:       user.Did,
		Rkey:      tid.TID(),
		Name:      name,
		ValueType: valueType,
		Scope:     scope,
		Color:     &color,
		Multiple:  multiple,
		Created:   time.Now(),
	}
	if err := rp.validator.ValidateLabelDefinition(&label); err != nil {
		fail(err.Error(), err)
		return
	}

	// announce this relation into the firehose, store into owners' pds
	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail(err.Error(), err)
		return
	}

	// emit a labelRecord
	labelRecord := label.AsRecord()
	resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.LabelDefinitionNSID,
		Repo:       label.Did,
		Rkey:       label.Rkey,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &labelRecord,
		},
	})
	// invalid record
	if err != nil {
		fail("Failed to write record to PDS.", err)
		return
	}

	aturi := resp.Uri
	l = l.With("at-uri", aturi)
	l.Info("wrote label record to PDS")

	// update the repo to subscribe to this label
	newRepo := *f
	newRepo.Labels = append(newRepo.Labels, aturi)
	repoRecord := newRepo.AsRecord()

	ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoNSID, newRepo.Did, newRepo.Rkey)
	if err != nil {
		fail("Failed to update labels, no record found on PDS.", err)
		return
	}
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoNSID,
		Repo:       newRepo.Did,
		Rkey:       newRepo.Rkey,
		SwapRecord: ex.Cid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &repoRecord,
		},
	})
	if err != nil {
		fail("Failed to update labels for repo.", err)
		return
	}

	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		fail("Failed to add label.", err)
		return
	}

	rollback := func() {
		err1 := tx.Rollback()
		err2 := rollbackRecord(context.Background(), aturi, client)

		// ignore txn complete errors, this is okay
		if errors.Is(err1, sql.ErrTxDone) {
			err1 = nil
		}

		if errs := errors.Join(err1, err2); errs != nil {
			l.Error("failed to rollback changes", "errs", errs)
			return
		}
	}
	defer rollback()

	_, err = db.AddLabelDefinition(tx, &label)
	if err != nil {
		fail("Failed to add label.", err)
		return
	}

	if err = db.SubscribeLabel(tx, &models.RepoLabel{
		RepoDid: syntax.DID(f.RepoDid),
		LabelAt: label.AtUri(),
	}); err != nil {
		fail("Failed to subscribe to label.", err)
		return
	}

	err = tx.Commit()
	if err != nil {
		fail("Failed to add label.", err)
		return
	}

	// clear aturi when everything is successful
	aturi = ""

	rp.pages.HxRefresh(w)
}

func (rp *Repo) DeleteLabelDef(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "DeleteLabel")
	l = l.With("did", user.Did)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	errorId := "label-operation"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	// get form values
	labelId := r.FormValue("label-id")

	label, err := db.GetLabelDefinition(rp.db, orm.FilterEq("id", labelId))
	if err != nil {
		fail("Failed to find label definition.", err)
		return
	}

	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail(err.Error(), err)
		return
	}

	// delete label record from PDS
	_, err = comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
		Collection: tangled.LabelDefinitionNSID,
		Repo:       label.Did,
		Rkey:       label.Rkey,
	})
	if err != nil {
		fail("Failed to delete label record from PDS.", err)
		return
	}

	// update repo record to remove the label reference
	newRepo := *f
	var updated []string
	removedAt := label.AtUri().String()
	for _, l := range newRepo.Labels {
		if l != removedAt {
			updated = append(updated, l)
		}
	}
	newRepo.Labels = updated
	repoRecord := newRepo.AsRecord()

	ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoNSID, newRepo.Did, newRepo.Rkey)
	if err != nil {
		fail("Failed to update labels, no record found on PDS.", err)
		return
	}
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoNSID,
		Repo:       newRepo.Did,
		Rkey:       newRepo.Rkey,
		SwapRecord: ex.Cid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &repoRecord,
		},
	})
	if err != nil {
		fail("Failed to update repo record.", err)
		return
	}

	// transaction for DB changes
	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		fail("Failed to delete label.", err)
		return
	}
	defer tx.Rollback()

	err = db.UnsubscribeLabel(
		tx,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("label_at", removedAt),
	)
	if err != nil {
		fail("Failed to unsubscribe label.", err)
		return
	}

	err = db.DeleteLabelDefinition(tx, orm.FilterEq("id", label.Id))
	if err != nil {
		fail("Failed to delete label definition.", err)
		return
	}

	err = tx.Commit()
	if err != nil {
		fail("Failed to delete label.", err)
		return
	}

	// everything succeeded
	rp.pages.HxRefresh(w)
}

func (rp *Repo) SubscribeLabel(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "SubscribeLabel")
	l = l.With("did", user.Did)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	if err := r.ParseForm(); err != nil {
		l.Error("invalid form", "err", err)
		return
	}

	errorId := "default-label-operation"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	labelAts := r.Form["label"]
	_, err = db.GetLabelDefinitions(rp.db, orm.FilterIn("at_uri", labelAts))
	if err != nil {
		fail("Failed to subscribe to label.", err)
		return
	}

	newRepo := *f
	newRepo.Labels = append(newRepo.Labels, labelAts...)

	// dedup
	slices.Sort(newRepo.Labels)
	newRepo.Labels = slices.Compact(newRepo.Labels)

	repoRecord := newRepo.AsRecord()

	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail(err.Error(), err)
		return
	}

	ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoNSID, f.Did, f.Rkey)
	if err != nil {
		fail("Failed to update labels, no record found on PDS.", err)
		return
	}
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoNSID,
		Repo:       newRepo.Did,
		Rkey:       newRepo.Rkey,
		SwapRecord: ex.Cid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &repoRecord,
		},
	})

	tx, err := rp.db.Begin()
	if err != nil {
		fail("Failed to subscribe to label.", err)
		return
	}
	defer tx.Rollback()

	for _, l := range labelAts {
		err = db.SubscribeLabel(tx, &models.RepoLabel{
			RepoDid: syntax.DID(f.RepoDid),
			LabelAt: syntax.ATURI(l),
		})
		if err != nil {
			fail("Failed to subscribe to label.", err)
			return
		}
	}

	if err := tx.Commit(); err != nil {
		fail("Failed to subscribe to label.", err)
		return
	}

	// everything succeeded
	rp.pages.HxRefresh(w)
}

func (rp *Repo) UnsubscribeLabel(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "UnsubscribeLabel")
	l = l.With("did", user.Did)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	if err := r.ParseForm(); err != nil {
		l.Error("invalid form", "err", err)
		return
	}

	errorId := "default-label-operation"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	labelAts := r.Form["label"]
	_, err = db.GetLabelDefinitions(rp.db, orm.FilterIn("at_uri", labelAts))
	if err != nil {
		fail("Failed to unsubscribe to label.", err)
		return
	}

	// update repo record to remove the label reference
	newRepo := *f
	var updated []string
	for _, l := range newRepo.Labels {
		if !slices.Contains(labelAts, l) {
			updated = append(updated, l)
		}
	}
	newRepo.Labels = updated
	repoRecord := newRepo.AsRecord()

	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail(err.Error(), err)
		return
	}

	ex, err := comatproto.RepoGetRecord(r.Context(), client, "", tangled.RepoNSID, f.Did, f.Rkey)
	if err != nil {
		fail("Failed to update labels, no record found on PDS.", err)
		return
	}
	_, err = comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoNSID,
		Repo:       newRepo.Did,
		Rkey:       newRepo.Rkey,
		SwapRecord: ex.Cid,
		Record: &lexutil.LexiconTypeDecoder{
			Val: &repoRecord,
		},
	})

	err = db.UnsubscribeLabel(
		rp.db,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterIn("label_at", labelAts),
	)
	if err != nil {
		fail("Failed to unsubscribe label.", err)
		return
	}

	// everything succeeded
	rp.pages.HxRefresh(w)
}

func (rp *Repo) LabelPanel(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "LabelPanel")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	subjectStr := r.FormValue("subject")
	subject, err := syntax.ParseATURI(subjectStr)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	labelDefs, err := db.GetLabelDefinitions(
		rp.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", subject.Collection().String()),
	)
	if err != nil {
		l.Error("failed to fetch label defs", "err", err)
		return
	}

	defs := make(map[string]*models.LabelDefinition)
	for _, l := range labelDefs {
		defs[l.AtUri().String()] = &l
	}

	states, err := db.GetLabels(rp.db, orm.FilterEq("subject", subject))
	if err != nil {
		l.Error("failed to build label state", "err", err)
		return
	}
	state := states[subject]

	user := rp.oauth.GetMultiAccountUser(r)
	rp.pages.LabelPanel(w, pages.LabelPanelParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Defs:       defs,
		Subject:    subject.String(),
		State:      state,
	})
}

func (rp *Repo) EditLabelPanel(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "EditLabelPanel")

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	subjectStr := r.FormValue("subject")
	subject, err := syntax.ParseATURI(subjectStr)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	labelDefs, err := db.GetLabelDefinitions(
		rp.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", subject.Collection().String()),
	)
	if err != nil {
		l.Error("failed to fetch labels", "err", err)
		return
	}

	defs := make(map[string]*models.LabelDefinition)
	for _, l := range labelDefs {
		defs[l.AtUri().String()] = &l
	}

	states, err := db.GetLabels(rp.db, orm.FilterEq("subject", subject))
	if err != nil {
		l.Error("failed to build label state", "err", err)
		return
	}
	state := states[subject]

	user := rp.oauth.GetMultiAccountUser(r)
	rp.pages.EditLabelPanel(w, pages.EditLabelPanelParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Defs:       defs,
		Subject:    subject.String(),
		State:      state,
	})
}

func (rp *Repo) AddCollaborator(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "AddCollaborator")
	l = l.With("did", user.Did)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	errorId := "add-collaborator-error"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	collaborator := r.FormValue("collaborator")
	if collaborator == "" {
		fail("Invalid form.", nil)
		return
	}

	// remove a single leading `@`, to make @handle work with ResolveIdent
	collaborator = strings.TrimPrefix(collaborator, "@")

	collaboratorIdent, err := rp.idResolver.ResolveIdent(r.Context(), collaborator)
	if err != nil {
		fail(fmt.Sprintf("'%s' is not a valid DID/handle.", collaborator), err)
		return
	}

	if collaboratorIdent.DID.String() == user.Did {
		fail("You seem to be adding yourself as a collaborator.", nil)
		return
	}
	l = l.With("collaborator", collaboratorIdent.Handle)
	l = l.With("knot", f.Knot)

	capStatus := knotcompat.KnotCapability(r.Context(), f.Knot, rp.config.Core.Dev, consts.CapKnotACL)
	if capStatus == knotcompat.CapUnknown {
		fail("Could not reach the knot to add the collaborator. Try again later.", nil)
		return
	}
	if capStatus == knotcompat.CapPresent {
		if f.RepoDid == "" {
			fail("This repository is missing its DID and cannot manage collaborators.", nil)
			return
		}

		client, err := rp.oauth.ServiceClient(
			r,
			oauth.WithService(f.Knot),
			oauth.WithLxm(tangled.RepoAddCollaboratorNSID),
			oauth.WithDev(rp.config.Core.Dev),
		)
		if err != nil {
			fail("Failed to connect to knot server.", err)
			return
		}

		err = tangled.RepoAddCollaborator(r.Context(), client, &tangled.RepoAddCollaborator_Input{
			Repo:    f.RepoDid,
			Subject: collaboratorIdent.DID.String(),
		})
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.addCollaborator", "xrpcerr", xrpcerr, "err", err)
			rp.pages.Notice(w, errorId, xrpcerr.Error())
			return
		}

		rp.acl.InvalidateCollaborators(f.Knot, f.RepoDid)

		rp.pages.HxRefresh(w)
		return
	}

	existing, err := db.GetCollaborators(rp.db,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("subject_did", collaboratorIdent.DID.String()),
	)
	if err != nil {
		fail("Failed to check existing collaborators.", err)
		return
	}
	if len(existing) > 0 {
		fail(fmt.Sprintf("%s is already a collaborator.", collaboratorIdent.Handle), nil)
		return
	}

	// announce this relation into the firehose, store into owners' pds
	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail("Failed to write to PDS.", err)
		return
	}

	// emit a record
	currentUser := rp.oauth.GetMultiAccountUser(r)
	rkey := tid.TID()
	createdAt := time.Now()
	resp, err := comatproto.RepoPutRecord(r.Context(), client, &comatproto.RepoPutRecord_Input{
		Collection: tangled.RepoCollaboratorNSID,
		Repo:       currentUser.Did,
		Rkey:       rkey,
		Record:     knotcompat.Collaborator(repoCollaboratorRecord(f, collaboratorIdent.DID.String(), createdAt)),
	})
	// invalid record
	if err != nil {
		fail("Failed to write record to PDS.", err)
		return
	}

	aturi := resp.Uri
	l = l.With("at-uri", aturi)
	l.Info("wrote record to PDS")

	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		fail("Failed to add collaborator.", err)
		return
	}

	rollback := func() {
		err1 := tx.Rollback()
		err2 := rp.enforcer.E.LoadPolicy()
		err3 := rollbackRecord(context.Background(), aturi, client)

		// ignore txn complete errors, this is okay
		if errors.Is(err1, sql.ErrTxDone) {
			err1 = nil
		}

		if errs := errors.Join(err1, err2, err3); errs != nil {
			l.Error("failed to rollback changes", "errs", errs)
			return
		}
	}
	defer rollback()

	err = rp.enforcer.AddCollaborator(collaboratorIdent.DID.String(), f.Knot, f.RepoIdentifier())
	if err != nil {
		fail("Failed to add collaborator permissions.", err)
		return
	}

	err = db.AddCollaborator(tx, models.Collaborator{
		Did:        syntax.DID(currentUser.Did),
		Rkey:       sql.NullString{String: rkey, Valid: true},
		SubjectDid: collaboratorIdent.DID,
		RepoDid:    syntax.DID(f.RepoDid),
		Created:    createdAt,
	})
	if err != nil {
		fail("Failed to add collaborator.", err)
		return
	}

	err = tx.Commit()
	if err != nil {
		fail("Failed to add collaborator.", err)
		return
	}

	err = rp.enforcer.E.SavePolicy()
	if err != nil {
		fail("Failed to update collaborator permissions.", err)
		return
	}

	// clear aturi to when everything is successful
	aturi = ""

	rp.pages.HxRefresh(w)
}

func (rp *Repo) RemoveCollaborator(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "RemoveCollaborator")
	l = l.With("did", user.Did)

	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	errorId := "collaborator-error"
	fail := func(msg string, err error) {
		l.Error(msg, "err", err)
		rp.pages.Notice(w, errorId, msg)
	}

	collaborator := r.FormValue("collaborator")
	if collaborator == "" {
		fail("Invalid form.", nil)
		return
	}
	collaborator = strings.TrimPrefix(collaborator, "@")

	collaboratorIdent, err := rp.idResolver.ResolveIdent(r.Context(), collaborator)
	if err != nil {
		fail(fmt.Sprintf("'%s' is not a valid DID/handle.", collaborator), err)
		return
	}
	l = l.With("collaborator", collaboratorIdent.Handle, "knot", f.Knot)

	if collaboratorIdent.DID.String() == f.Did {
		fail("Cannot remove the repository owner.", nil)
		return
	}

	capStatus := knotcompat.KnotCapability(r.Context(), f.Knot, rp.config.Core.Dev, consts.CapKnotACL)
	if capStatus == knotcompat.CapUnknown {
		fail("Could not reach the knot to remove the collaborator. Try again later.", nil)
		return
	}
	if capStatus == knotcompat.CapPresent {
		if f.RepoDid == "" {
			fail("This repository is missing its DID and cannot manage collaborators.", nil)
			return
		}

		client, err := rp.oauth.ServiceClient(
			r,
			oauth.WithService(f.Knot),
			oauth.WithLxm(tangled.RepoRemoveCollaboratorNSID),
			oauth.WithDev(rp.config.Core.Dev),
		)
		if err != nil {
			fail("Failed to connect to knot server.", err)
			return
		}

		err = tangled.RepoRemoveCollaborator(r.Context(), client, &tangled.RepoRemoveCollaborator_Input{
			Repo:    f.RepoDid,
			Subject: collaboratorIdent.DID.String(),
		})
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.removeCollaborator", "xrpcerr", xrpcerr, "err", err)
			rp.pages.Notice(w, errorId, xrpcerr.Error())
			return
		}

		rp.acl.InvalidateCollaborators(f.Knot, f.RepoDid)

		rp.pages.HxRefresh(w)
		return
	}

	existing, err := db.GetCollaborators(rp.db,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("subject_did", collaboratorIdent.DID.String()),
	)
	if err != nil {
		fail("Failed to look up collaborator.", err)
		return
	}
	if len(existing) == 0 {
		fail(fmt.Sprintf("%s is not a collaborator.", collaboratorIdent.Handle), nil)
		return
	}
	row := existing[0]

	client, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		fail("Failed to write to PDS.", err)
		return
	}

	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		fail("Failed to remove collaborator.", err)
		return
	}
	committed := false
	defer func() {
		if !committed {
			tx.Rollback()
			if err := rp.enforcer.E.LoadPolicy(); err != nil {
				l.Error("failed to reload policy after rollback", "err", err)
			}
		}
	}()

	if err := rp.enforcer.RemoveCollaborator(collaboratorIdent.DID.String(), f.Knot, f.RepoIdentifier()); err != nil {
		fail("Failed to remove collaborator permissions.", err)
		return
	}

	if err := db.DeleteCollaborator(tx,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("subject_did", collaboratorIdent.DID.String()),
	); err != nil {
		fail("Failed to remove collaborator.", err)
		return
	}

	if row.Rkey.Valid && row.Rkey.String != "" {
		if _, err := comatproto.RepoDeleteRecord(r.Context(), client, &comatproto.RepoDeleteRecord_Input{
			Collection: tangled.RepoCollaboratorNSID,
			Repo:       row.Did.String(),
			Rkey:       row.Rkey.String,
		}); err != nil {
			fail("Failed to delete collaborator record from PDS.", err)
			return
		}
	}

	if err := tx.Commit(); err != nil {
		fail("Failed to remove collaborator.", err)
		return
	}
	committed = true

	if err := rp.enforcer.E.SavePolicy(); err != nil {
		fail("Failed to update collaborator permissions.", err)
		return
	}

	rp.pages.HxRefresh(w)
}

func (rp *Repo) RenameRepo(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "RenameRepo")
	noticeId := "rename-repo-error"

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		rp.pages.Notice(w, noticeId, "Failed to load repository.")
		return
	}
	l = l.With("did", user.Did, "rkey", f.Rkey, "oldName", f.Name)

	if f.RepoDid == "" {
		rp.pages.Notice(w, noticeId, "This repository's knot has not completed the DID migration; rename is unavailable.")
		return
	}

	if !knotcompat.KnotSupports114(r.Context(), f.Knot, rp.config.Core.Dev) {
		rp.pages.Notice(w, noticeId, "This repository's knot is below v1.14 and does not yet support renames. Ask the knot operator to upgrade.")
		return
	}

	newName, err := validateRenameInput(f.Name, f.Rkey, r.FormValue("name"))
	if err != nil {
		rp.pages.Notice(w, noticeId, err.Error())
		return
	}
	newRkey := strings.ToLower(newName)
	l = l.With("newName", newName, "newRkey", newRkey)

	atpClient, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		rp.pages.Notice(w, noticeId, "Failed to authorize. Try again later.")
		return
	}

	newRepo := *f
	newRepo.Name = newName
	newRepo.Rkey = newRkey
	newRepo.Created = time.Now()
	record := newRepo.AsRecord()

	if newRkey == f.Rkey {
		ex, err := comatproto.RepoGetRecord(r.Context(), atpClient, "", tangled.RepoNSID, f.Did, f.Rkey)
		if err != nil {
			l.Error("failed to fetch existing record", "err", err)
			rp.pages.Notice(w, noticeId, "Failed to read repository record from PDS.")
			return
		}

		_, err = comatproto.RepoPutRecord(r.Context(), atpClient, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoNSID,
			Repo:       f.Did,
			Rkey:       f.Rkey,
			SwapRecord: ex.Cid,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &record,
			},
		})
		if err != nil {
			l.Error("failed to update display name on PDS", "err", err)
			rp.pages.Notice(w, noticeId, "Failed to save display name to PDS.")
			return
		}
		l.Info("updated display name on PDS")

		if err := db.UpdateRepoDisplayName(rp.db, f.Did, f.Rkey, newName); err != nil {
			l.Error("optimistic display name update failed", "err", err)
		}
	} else {
		ex, getErr := comatproto.RepoGetRecord(r.Context(), atpClient, "", tangled.RepoNSID, f.Did, newRkey)
		switch {
		case getErr != nil:
			_, err = comatproto.RepoCreateRecord(r.Context(), atpClient, &comatproto.RepoCreateRecord_Input{
				Collection: tangled.RepoNSID,
				Repo:       f.Did,
				Rkey:       &newRkey,
				Record:     &lexutil.LexiconTypeDecoder{Val: &record},
			})
			if err != nil {
				l.Error("failed to write rename to PDS", "err", err)
				rp.pages.Notice(w, noticeId, "Failed to save renamed repository to PDS.")
				return
			}
			l.Info("wrote rename-create to PDS; old record retained as alias")

		default:
			existing, ok := ex.Value.Val.(*tangled.Repo)
			if !ok || existing.RepoDid == nil || *existing.RepoDid != f.RepoDid {
				rp.pages.Notice(w, noticeId, fmt.Sprintf("You already have a repository named %q.", newRkey))
				return
			}
			_, err = comatproto.RepoPutRecord(r.Context(), atpClient, &comatproto.RepoPutRecord_Input{
				Collection: tangled.RepoNSID,
				Repo:       f.Did,
				Rkey:       newRkey,
				SwapRecord: ex.Cid,
				Record:     &lexutil.LexiconTypeDecoder{Val: &record},
			})
			if err != nil {
				l.Error("failed to rewrite rename-back record on PDS", "err", err)
				rp.pages.Notice(w, noticeId, "Failed to save renamed repository to PDS.")
				return
			}
			l.Info("rewrote rename-back record on PDS over prior alias")
		}

		tx, err := rp.db.Begin()
		if err != nil {
			l.Error("failed to begin rename tx", "err", err)
			rp.pages.HxLocation(w, fmt.Sprintf("/%s", f.RepoDid))
			return
		}
		defer tx.Rollback()

		if err := db.RenameRepo(tx, f.Did, f.Rkey, newRkey, newName); err != nil {
			l.Error("optimistic rename failed", "err", err)
			rp.pages.HxLocation(w, fmt.Sprintf("/%s", f.RepoDid))
			return
		}
		if err := db.RecordRepoRename(tx, f.Did, f.Rkey, f.RepoDid); err != nil {
			l.Error("failed to record rename history", "err", err)
		}
		if err := db.DeleteRepoRename(tx, f.Did, newRkey); err != nil {
			l.Error("failed to clear stale rename hint", "err", err)
		}
		if err := tx.Commit(); err != nil {
			l.Error("failed to commit rename tx", "err", err)
			rp.pages.HxLocation(w, fmt.Sprintf("/%s", f.RepoDid))
			return
		}
	}

	oldRepo := *f
	rp.notifier.RenameRepo(r.Context(), syntax.DID(user.Did), &oldRepo, &newRepo)

	if newRkey != f.Rkey {
		rp.migrateSiteOnRename(r.Context(), f, newName, newRkey)
	}

	rp.pages.HxLocation(w, fmt.Sprintf("/%s", f.RepoDid))
}

func validateRenameInput(currentName, currentRkey, raw string) (string, error) {
	newName := strings.TrimSpace(raw)
	if newName == "" {
		return "", errors.New("Repository name cannot be empty.")
	}
	if err := models.ValidateRepoName(newName); err != nil {
		return "", err
	}
	newName = models.StripGitExt(newName)
	if newName == currentName {
		if _, tidErr := syntax.ParseTID(currentRkey); tidErr == nil {
			return newName, nil
		}
		return "", errors.New("New name matches the current name.")
	}
	return newName, nil
}

func (rp *Repo) migrateSiteOnRename(ctx context.Context, oldRepo *models.Repo, newName, newRkey string) {
	l := rp.logger.With("handler", "migrateSiteOnRename", "repo_did", oldRepo.RepoDid)

	siteConfig, err := db.GetRepoSiteConfig(rp.db, oldRepo.RepoDid)
	if err != nil || siteConfig == nil {
		return
	}

	if !rp.cfClient.Enabled() {
		return
	}

	ownerClaim, _ := db.GetActiveDomainClaimForDid(rp.db, oldRepo.Did)

	go func() {
		bgCtx := context.Background()
		oldRkey := oldRepo.Rkey
		oldName := oldRepo.Name

		if err := sites.Delete(bgCtx, rp.cfClient, oldRepo.Did, oldRkey); err != nil {
			l.Error("sites: failed to delete old R2 prefix", "oldRkey", oldRkey, "err", err)
		}

		newRepo := *oldRepo
		newRepo.Name = newName
		newRepo.Rkey = newRkey
		if deployErr := sites.Deploy(bgCtx, rp.cfClient, rp.config, &newRepo, siteConfig.Branch, siteConfig.Dir); deployErr != nil {
			l.Error("sites: redeploy after rename failed", "err", deployErr)
		}

		if ownerClaim != nil {
			// drop the old name's entry when the name actually changed.
			if oldName != newName {
				if err := sites.DeleteDomainMapping(bgCtx, rp.cfClient, ownerClaim.Domain, oldName); err != nil {
					l.Error("sites: failed to remove old KV mapping", "oldName", oldName, "err", err)
				}
			}
			if err := sites.PutDomainMapping(bgCtx, rp.cfClient, ownerClaim.Domain, oldRepo.Did, newName, newRkey, siteConfig.IsIndex); err != nil {
				l.Error("sites: failed to write new KV mapping", "newName", newName, "newRkey", newRkey, "err", err)
			}
		}

		l.Info("sites: migrated on rename", "oldName", oldName, "oldRkey", oldRkey, "newName", newName, "newRkey", newRkey)
	}()
}

func (rp *Repo) DeleteRepo(w http.ResponseWriter, r *http.Request) {
	user := rp.oauth.GetMultiAccountUser(r)
	l := rp.logger.With("handler", "DeleteRepo")

	noticeId := "operation-error"
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to get repo and knot", "err", err)
		return
	}

	// remove record from pds
	atpClient, err := rp.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		return
	}
	_, err = comatproto.RepoDeleteRecord(r.Context(), atpClient, &comatproto.RepoDeleteRecord_Input{
		Collection: tangled.RepoNSID,
		Repo:       user.Did,
		Rkey:       f.Rkey,
	})
	if err != nil {
		l.Error("failed to delete record", "err", err)
		rp.pages.Notice(w, noticeId, "Failed to delete repository from PDS.")
		return
	}
	l.Info("removed repo record", "aturi", f.RepoAt().String())

	client, err := rp.oauth.ServiceClient(
		r,
		oauth.WithService(f.Knot),
		oauth.WithLxm(tangled.RepoDeleteNSID),
		oauth.WithDev(rp.config.Core.Dev),
	)
	if err != nil {
		l.Error("failed to connect to knot server", "err", err)
		return
	}

	err = tangled.RepoDelete(
		r.Context(),
		client,
		&tangled.RepoDelete_Input{
			Did:  f.Did,
			Name: f.Name,
			Rkey: f.Rkey,
		},
	)
	if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
		l.Error("failed to call XRPC repo.delete", "xrpcerr", xrpcerr, "err", err)
		rp.pages.Notice(w, noticeId, xrpcerr.Error())
		return
	}
	l.Info("deleted repo from knot")

	tx, err := rp.db.BeginTx(r.Context(), nil)
	if err != nil {
		l.Error("failed to start tx")
		w.Write(fmt.Append(nil, "failed to add collaborator: ", err))
		return
	}
	defer func() {
		tx.Rollback()
		err = rp.enforcer.E.LoadPolicy()
		if err != nil {
			l.Error("failed to rollback policies")
		}
	}()

	// remove collaborator RBAC
	repoCollaborators, err := rp.enforcer.E.GetImplicitUsersForResourceByDomain(f.RepoIdentifier(), f.Knot)
	if err != nil {
		rp.pages.Notice(w, noticeId, "Failed to remove collaborators")
		return
	}
	for _, c := range repoCollaborators {
		did := c[0]
		rp.enforcer.RemoveCollaborator(did, f.Knot, f.RepoIdentifier())
	}
	l.Info("removed collaborators")

	// remove repo RBAC
	err = rp.enforcer.RemoveRepo(f.Did, f.Knot, f.RepoIdentifier())
	if err != nil {
		rp.pages.Notice(w, noticeId, "Failed to update RBAC rules")
		return
	}

	// remove repo from db
	err = db.RemoveRepo(tx, f.Did, f.Rkey)
	if err != nil {
		rp.pages.Notice(w, noticeId, "Failed to update appview")
		return
	}
	l.Info("removed repo from db")

	err = tx.Commit()
	if err != nil {
		l.Error("failed to commit changes", "err", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	err = rp.enforcer.E.SavePolicy()
	if err != nil {
		l.Error("failed to update ACLs", "err", err)
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	rp.notifier.DeleteRepo(r.Context(), f)
	rp.pages.HxRedirect(w, fmt.Sprintf("/%s", f.Did))
}

func (rp *Repo) SyncRepoFork(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "SyncRepoFork")

	ref := chi.URLParam(r, "ref")
	ref, _ = url.PathUnescape(ref)

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve source repo", "err", err)
		return
	}

	switch r.Method {
	case http.MethodPost:
		client, err := rp.oauth.ServiceClient(
			r,
			oauth.WithService(f.Knot),
			oauth.WithLxm(tangled.RepoForkSyncNSID),
			oauth.WithDev(rp.config.Core.Dev),
		)
		if err != nil {
			rp.pages.Notice(w, "repo", "Failed to connect to knot server.")
			return
		}

		if f.Source == "" {
			rp.pages.Notice(w, "repo", "This repository is not a fork.")
			return
		}

		err = tangled.RepoForkSync(
			r.Context(),
			client,
			&tangled.RepoForkSync_Input{
				Did:    user.Did,
				Name:   f.Name,
				Source: f.Source,
				Branch: ref,
			},
		)
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.forkSync", "xrpcerr", xrpcerr, "err", err)
			rp.pages.Notice(w, "repo", err.Error())
			return
		}

		rp.pages.HxRefresh(w)
		return
	}
}

func (rp *Repo) ForkRepo(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "ForkRepo")

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve source repo", "err", err)
		return
	}

	switch r.Method {
	case http.MethodGet:
		user := rp.oauth.GetMultiAccountUser(r)
		knots := rp.acl.KnotsForUser(r.Context(), user.Did)

		rp.pages.ForkRepo(w, pages.ForkRepoParams{
			BaseParams: pages.BaseParamsFromContext(r.Context()),
			Knots:      knots,
			RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		})

	case http.MethodPost:
		l := rp.logger.With("handler", "ForkRepo")

		targetKnot := r.FormValue("knot")
		if targetKnot == "" {
			rp.pages.Notice(w, "repo", "Invalid form submission&mdash;missing knot domain.")
			return
		}
		l = l.With("targetKnot", targetKnot)

		if !rp.acl.IsRepoCreateAllowed(r.Context(), targetKnot, user.Did) {
			rp.pages.Notice(w, "repo", "You do not have permission to create a repo in this knot.")
			return
		}

		// choose a name for a fork
		forkName := strings.ToLower(r.FormValue("repo_name"))
		if forkName == "" {
			rp.pages.Notice(w, "repo", "Repository name cannot be empty.")
			return
		}

		// this check is *only* to see if the forked repo name already exists
		// in the user's account.
		existingRepo, err := db.GetRepo(
			rp.db,
			orm.FilterEq("did", user.Did),
			orm.FilterEq("name", forkName),
		)
		if err != nil {
			if !errors.Is(err, sql.ErrNoRows) {
				l.Error("error fetching existing repo from db", "err", err)
				rp.pages.Notice(w, "repo", "Failed to fork this repository. Try again later.")
				return
			}
		} else if existingRepo != nil {
			// repo with this name already exists
			rp.pages.Notice(w, "repo", "A repository with this name already exists.")
			return
		}
		l = l.With("forkName", forkName)

		uri := "https"
		if rp.config.Core.Dev {
			uri = "http"
		}

		forkSourceUrl := fmt.Sprintf("%s://%s/%s", uri, f.Knot, f.RepoIdentifier())
		l = l.With("cloneUrl", forkSourceUrl)

		rkey := strings.ToLower(forkName)

		// TODO: this could coordinate better with the knot to receive a clone status
		client, err := rp.oauth.ServiceClient(
			r,
			oauth.WithService(targetKnot),
			oauth.WithLxm(tangled.RepoCreateNSID),
			oauth.WithDev(rp.config.Core.Dev),
			oauth.WithTimeout(time.Second*20),
		)
		if err != nil {
			l.Error("could not create service client", "err", err)
			rp.pages.Notice(w, "repo", "Failed to connect to knot server.")
			return
		}

		forkInput := &tangled.RepoCreate_Input{
			Rkey:   rkey,
			Name:   rkey,
			Source: &forkSourceUrl,
		}
		createResp, err := tangled.RepoCreate(
			r.Context(),
			client,
			forkInput,
		)
		if xrpcerr := xrpcclient.HandleXrpcErr(err); xrpcerr != nil {
			l.Error("failed to call XRPC repo.create", "xrpcerr", xrpcerr, "err", err)
			rp.pages.Notice(w, "repo", xrpcerr.Error())
			return
		}

		var repoDid string
		if createResp != nil && createResp.RepoDid != nil {
			repoDid = *createResp.RepoDid
		}
		if repoDid == "" {
			l.Error("knot returned empty repo DID for fork")
			rp.pages.Notice(w, "repo", "Knot failed to mint a repo DID. The knot may need to be upgraded.")
			return
		}

		forkSource := f.RepoAt().String()
		if f.RepoDid != "" {
			forkSource = f.RepoDid
		}

		forkDescription := r.Form.Get("description")

		repo := &models.Repo{
			Did:         user.Did,
			Name:        rkey,
			Knot:        targetKnot,
			Rkey:        rkey,
			Source:      forkSource,
			Description: forkDescription,
			Created:     time.Now(),
			Labels:      rp.config.Label.DefaultLabelDefs,
			RepoDid:     repoDid,
		}
		record := repo.AsRecord()

		cleanupKnot := func() {
			go func() {
				delays := []time.Duration{0, 2 * time.Second, 5 * time.Second}
				for attempt, delay := range delays {
					time.Sleep(delay)
					deleteClient, dErr := rp.oauth.ServiceClient(
						r,
						oauth.WithService(targetKnot),
						oauth.WithLxm(tangled.RepoDeleteNSID),
						oauth.WithDev(rp.config.Core.Dev),
					)
					if dErr != nil {
						l.Error("failed to create delete client for knot cleanup", "attempt", attempt+1, "err", dErr)
						continue
					}
					ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
					if dErr := tangled.RepoDelete(ctx, deleteClient, &tangled.RepoDelete_Input{
						Did:  user.Did,
						Name: forkName,
						Rkey: rkey,
					}); dErr != nil {
						cancel()
						l.Error("failed to clean up fork on knot after rollback", "attempt", attempt+1, "err", dErr)
						continue
					}
					cancel()
					l.Info("successfully cleaned up fork on knot after rollback", "attempt", attempt+1)
					return
				}
				l.Error("exhausted retries for knot cleanup, fork may be orphaned",
					"did", user.Did, "fork", forkName, "knot", targetKnot)
			}()
		}

		atpClient, err := rp.oauth.AuthorizedClient(r)
		if err != nil {
			l.Error("failed to create xrpcclient", "err", err)
			cleanupKnot()
			rp.pages.Notice(w, "repo", "Failed to fork repository.")
			return
		}

		atresp, err := comatproto.RepoPutRecord(r.Context(), atpClient, &comatproto.RepoPutRecord_Input{
			Collection: tangled.RepoNSID,
			Repo:       user.Did,
			Rkey:       rkey,
			Record: &lexutil.LexiconTypeDecoder{
				Val: &record,
			},
		})
		if err != nil {
			l.Error("failed to write to PDS", "err", err)
			cleanupKnot()
			rp.pages.Notice(w, "repo", "Failed to announce repository creation.")
			return
		}

		aturi := atresp.Uri
		l = l.With("aturi", aturi)
		l.Info("wrote to PDS")

		tx, err := rp.db.BeginTx(r.Context(), nil)
		if err != nil {
			l.Info("txn failed", "err", err)
			rp.pages.Notice(w, "repo", "Failed to save repository information.")
			return
		}

		rollback := func() {
			err1 := tx.Rollback()
			err2 := rp.enforcer.E.LoadPolicy()
			err3 := rollbackRecord(context.Background(), aturi, atpClient)

			if errors.Is(err1, sql.ErrTxDone) {
				err1 = nil
			}

			if errs := errors.Join(err1, err2, err3); errs != nil {
				l.Error("failed to rollback changes", "errs", errs)
			}

			if aturi != "" {
				cleanupKnot()
			}
		}
		defer rollback()

		err = db.AddRepo(tx, repo)
		if err != nil {
			l.Error("failed to AddRepo", "err", err)
			rp.pages.Notice(w, "repo", "Failed to save repository information.")
			return
		}

		rbacPath := repo.RepoIdentifier()
		err = rp.enforcer.AddRepo(user.Did, targetKnot, rbacPath)
		if err != nil {
			l.Error("failed to add ACLs", "err", err)
			rp.pages.Notice(w, "repo", "Failed to set up repository permissions.")
			return
		}

		err = tx.Commit()
		if err != nil {
			l.Error("failed to commit changes", "err", err)
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		err = rp.enforcer.E.SavePolicy()
		if err != nil {
			l.Error("failed to update ACLs", "err", err)
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}

		aturi = ""

		rp.notifier.NewRepo(r.Context(), repo)
		if repoDid != "" {
			rp.pages.HxLocation(w, fmt.Sprintf("/%s", repoDid))
		} else {
			rp.pages.HxLocation(w, fmt.Sprintf("/%s/%s", user.Did, forkName))
		}
	}
}

func (rp *Repo) Stars(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "Stars")

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve source repo", "err", err)
		return
	}

	page := pagination.FromContext(r.Context())
	if page.Limit > 30 || page.Limit <= 0 {
		page.Limit = 30
	}

	starrers, err := db.GetStars(rp.db, string(f.RepoDid), page)
	if err != nil {
		l.Error("failed to fetch starrers", "err", err, "repoDid", f.RepoDid)
		return
	}

	totalCount, err := db.GetStarCount(rp.db, models.StarSubjectRepo, string(f.RepoDid))
	if err != nil {
		l.Error("failed to fetch star count", "err", err, "repoDid", f.RepoDid)
		return
	}

	rp.pages.RepoStars(w, pages.RepoStarsParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Starrers:   starrers,
		Page:       page,
		TotalCount: totalCount,
	})
}

func (rp *Repo) Forks(w http.ResponseWriter, r *http.Request) {
	l := rp.logger.With("handler", "Forks")

	user := rp.oauth.GetMultiAccountUser(r)
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		l.Error("failed to resolve source repo", "err", err)
		return
	}

	var forks []models.Repo
	totalCount := 0
	page := pagination.FromContext(r.Context())
	if f.RepoDid != "" {
		forks, err = db.GetReposPaginated(rp.db, page, orm.FilterEq("source", f.RepoDid))
		if err != nil {
			l.Error("failed to fetch forks", "err", err, "repoAt", f.RepoAt())
			return
		}

		totalCount, err = db.GetForkCount(rp.db, f.RepoDid)
		if err != nil {
			l.Error("failed to fetch fork count", "err", err, "repoAt", f.RepoAt())
			return
		}
	}

	err = rp.pages.RepoForks(w, pages.RepoForksParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		RepoInfo:   rp.repoResolver.GetRepoInfo(r, user),
		Forks:      forks,
		Page:       page,
		TotalCount: totalCount,
	})
	if err != nil {
		l.Error("failed to render page", "err", err)
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

func repoCollaboratorRecord(f *models.Repo, subject string, createdAt time.Time) *tangled.RepoCollaborator {
	return &tangled.RepoCollaborator{
		Subject:   subject,
		CreatedAt: createdAt.Format(time.RFC3339),
		Repo:      f.RepoDid,
	}
}
