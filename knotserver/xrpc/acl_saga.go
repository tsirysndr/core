package xrpc

import (
	"context"
	"database/sql"
	"log/slog"
	"net/http"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/keys"
	xrpcerr "tangled.org/core/xrpc/errors"
)

type aclGrant struct {
	role      string
	subject   syntax.DID
	inAcl     func() (bool, error)
	inTable   func() (bool, error)
	insertRow func(*sql.Tx) error
	deleteRow func() error
	grantAcl  func() error
	emit      func() error
}

type aclRevoke struct {
	role       string
	subject    syntax.DID
	inAcl      func() (bool, error)
	inTable    func() (bool, error)
	removeAcl  func() (bool, error)
	restoreAcl func() error
	deleteRow  func(*sql.Tx) error
	emit       func() error
}

func (h *Xrpc) applyAclGrant(ctx context.Context, l *slog.Logger, g aclGrant) (int, *xrpcerr.XrpcError) {
	fail := func(status int, e xrpcerr.XrpcError) (int, *xrpcerr.XrpcError) {
		return status, &e
	}

	inAcl, err := g.inAcl()
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	inTable, err := g.inTable()
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if inAcl && inTable {
		l.Info("subject already granted, no-op", "role", g.role, "subject", g.subject)
		return http.StatusOK, nil
	}

	didKnown, err := db.IsDidKnown(h.Db, g.subject.String())
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}

	tx, err := h.Db.BeginTx(ctx, nil)
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	committed := false
	defer func() {
		if !committed {
			tx.Rollback()
		}
	}()

	if err := db.AddDid(tx, g.subject.String()); err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if err := g.insertRow(tx); err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if err := tx.Commit(); err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	committed = true

	if err := g.grantAcl(); err != nil {
		if !inTable {
			if rbErr := g.deleteRow(); rbErr != nil {
				l.Error("failed to roll back row after ACL grant failed", "role", g.role, "subject", g.subject, "error", rbErr)
			}
		}
		if !didKnown {
			if rbErr := db.RemoveDid(h.Db, g.subject.String()); rbErr != nil {
				l.Error("failed to roll back known_did after ACL grant failed", "role", g.role, "subject", g.subject, "error", rbErr)
			}
		}
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}

	h.Ingester.AddDid(g.subject.String())
	h.fetchKeysAsync(ctx, l, g.subject)

	if g.emit != nil {
		if err := g.emit(); err != nil {
			l.Error("failed to emit acl grant event, appview reconcile will catch up", "role", g.role, "subject", g.subject, "error", err)
		}
	}

	l.Info("granted", "role", g.role, "subject", g.subject)
	return http.StatusOK, nil
}

func (h *Xrpc) applyAclRevoke(ctx context.Context, l *slog.Logger, rv aclRevoke) (int, *xrpcerr.XrpcError) {
	fail := func(status int, e xrpcerr.XrpcError) (int, *xrpcerr.XrpcError) {
		return status, &e
	}

	inAcl, err := rv.inAcl()
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	inTable, err := rv.inTable()
	if err != nil {
		return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if !inAcl && !inTable {
		l.Info("subject not granted, no-op", "role", rv.role, "subject", rv.subject)
		return http.StatusOK, nil
	}

	removed := false
	if inAcl {
		removed, err = rv.removeAcl()
		if err != nil {
			return fail(http.StatusInternalServerError, xrpcerr.GenericError(err))
		}
	}

	failRestoringACL := func(status int, e xrpcerr.XrpcError) (int, *xrpcerr.XrpcError) {
		if removed {
			if rbErr := rv.restoreAcl(); rbErr != nil {
				l.Error("failed to restore ACL after remove rollback", "role", rv.role, "subject", rv.subject, "error", rbErr)
			}
		}
		return fail(status, e)
	}

	stillKnown, err := h.Enforcer.HasAnyPolicyForUser(rv.subject.String())
	if err != nil {
		return failRestoringACL(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}

	tx, err := h.Db.BeginTx(ctx, nil)
	if err != nil {
		return failRestoringACL(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	committed := false
	defer func() {
		if !committed {
			tx.Rollback()
		}
	}()

	if err := rv.deleteRow(tx); err != nil {
		return failRestoringACL(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	if !stillKnown {
		if err := db.RemoveDid(tx, rv.subject.String()); err != nil {
			return failRestoringACL(http.StatusInternalServerError, xrpcerr.GenericError(err))
		}
	}
	if err := tx.Commit(); err != nil {
		return failRestoringACL(http.StatusInternalServerError, xrpcerr.GenericError(err))
	}
	committed = true

	if !stillKnown {
		h.Ingester.RemoveDid(rv.subject.String())
	}

	if rv.emit != nil {
		if err := rv.emit(); err != nil {
			l.Error("failed to emit acl revoke event, appview reconcile will catch up", "role", rv.role, "subject", rv.subject, "error", err)
		}
	}

	l.Info("revoked", "role", rv.role, "subject", rv.subject, "did_dropped", !stillKnown)
	return http.StatusOK, nil
}

func (h *Xrpc) fetchKeysAsync(ctx context.Context, l *slog.Logger, subject syntax.DID) {
	kctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), keyFetchTimeout)
	go func() {
		defer cancel()
		if err := keys.FetchAndStore(kctx, h.Resolver.Directory(), h.Db, subject.String()); err != nil {
			l.Warn("failed to fetch subject public keys, continuing", "subject", subject, "error", err)
		}
	}()
}
