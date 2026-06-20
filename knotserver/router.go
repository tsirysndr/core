package knotserver

import (
	"context"
	_ "embed"
	"fmt"
	"log/slog"
	"net/http"
	"strings"
	"sync"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/go-chi/chi/v5"
	"tangled.org/core/idresolver"
	"tangled.org/core/jetstream"
	"tangled.org/core/knotserver/config"
	"tangled.org/core/knotserver/db"
	"tangled.org/core/knotserver/keys"
	"tangled.org/core/knotserver/sandbox"
	"tangled.org/core/knotserver/xrpc"
	"tangled.org/core/log"
	"tangled.org/core/notifier"
	"tangled.org/core/rbac"
	"tangled.org/core/xrpc/serviceauth"
)

//go:embed motd
var defaultMotd []byte

type Knot struct {
	c        *config.Config
	db       *db.DB
	jc       *jetstream.JetstreamClient
	e        *rbac.Enforcer
	l        *slog.Logger
	n        *notifier.Notifier
	resolver *idresolver.Resolver
	sandbox  sandbox.Backend
	motd     []byte
	motdMu   sync.RWMutex
}

func Setup(ctx context.Context, c *config.Config, db *db.DB, e *rbac.Enforcer, jc *jetstream.JetstreamClient, n *notifier.Notifier, resolver *idresolver.Resolver, sb sandbox.Backend) (http.Handler, error) {
	h := Knot{
		c:        c,
		db:       db,
		e:        e,
		l:        log.FromContext(ctx),
		jc:       jc,
		n:        n,
		resolver: resolver,
		sandbox:  sb,
		motd:     defaultMotd,
	}

	err := e.AddKnot(rbac.ThisServer)
	if err != nil {
		return nil, fmt.Errorf("failed to setup enforcer: %w", err)
	}

	// configure owner
	if err = h.configureOwner(ctx); err != nil {
		return nil, err
	}
	h.l.Info("owner set", "did", h.c.Server.Owner)
	h.jc.AddDid(h.c.Server.Owner)

	// configure known-dids in jetstream consumer
	dids, err := h.db.GetAllDids()
	if err != nil {
		return nil, fmt.Errorf("failed to get all dids: %w", err)
	}
	for _, d := range dids {
		jc.AddDid(d)
	}

	err = h.jc.StartJetstream(ctx, h.processMessages)
	if err != nil {
		return nil, fmt.Errorf("failed to start jetstream: %w", err)
	}

	return h.Router(), nil
}

func (h *Knot) Router() http.Handler {
	r := chi.NewRouter()

	r.Use(h.CORS)
	r.Use(h.RequestLogger)

	r.Get("/", func(w http.ResponseWriter, r *http.Request) {
		w.Write(h.GetMotdContent())
	})

	r.Route("/{did}", func(r chi.Router) {
		r.Use(h.resolveDidRedirect)

		r.Get("/info/refs", h.InfoRefs)
		r.Post("/git-upload-archive", h.UploadArchive)
		r.Post("/git-upload-pack", h.UploadPack)
		r.Post("/git-receive-pack", h.ReceivePack)

		r.Route("/{name}", func(r chi.Router) {
			r.Get("/info/refs", h.InfoRefs)
			r.Post("/git-upload-archive", h.UploadArchive)
			r.Post("/git-upload-pack", h.UploadPack)
			r.Post("/git-receive-pack", h.ReceivePack)
		})
	})

	// xrpc apis
	x := h.newXrpc()
	r.Mount("/xrpc", x.Router())
	r.Mount("/admin", x.AdminRouter())

	// Socket that streams git oplogs
	r.Get("/events", h.Events)

	return r
}

// SetMotdContent sets custom MOTD content, replacing the embedded default.
func (h *Knot) SetMotdContent(content []byte) {
	h.motdMu.Lock()
	defer h.motdMu.Unlock()
	h.motd = content
}

// GetMotdContent returns the current MOTD content.
func (h *Knot) GetMotdContent() []byte {
	h.motdMu.RLock()
	defer h.motdMu.RUnlock()
	return h.motd
}

func (h *Knot) newXrpc() *xrpc.Xrpc {
	serviceAuth := serviceauth.NewServiceAuth(h.l, h.resolver.Directory(), h.c.Server.Did().String())

	l := log.SubLogger(h.l, "xrpc")

	return &xrpc.Xrpc{
		Config:      h.c,
		Db:          h.db,
		Ingester:    h.jc,
		Enforcer:    h.e,
		Logger:      l,
		Notifier:    h.n,
		Resolver:    h.resolver,
		ServiceAuth: serviceAuth,
		Sandbox:     h.sandbox,
	}
}

func (h *Knot) resolveDidRedirect(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		didOrHandle := chi.URLParam(r, "did")
		if strings.HasPrefix(didOrHandle, "did:") {
			next.ServeHTTP(w, r)
			return
		}

		trimmed := strings.TrimPrefix(didOrHandle, "@")
		id, err := h.resolver.ResolveIdent(r.Context(), trimmed)
		if err != nil {
			// invalid did or handle
			h.l.Error("failed to resolve did/handle", "handle", trimmed, "err", err)
			http.Error(w, fmt.Sprintf("failed to resolve did/handle: %s", trimmed), http.StatusInternalServerError)
			return
		}

		suffix := strings.TrimPrefix(r.URL.Path, "/"+didOrHandle)
		newPath := "/" + id.DID.String() + suffix
		if r.URL.RawQuery != "" {
			newPath += "?" + r.URL.RawQuery
		}
		http.Redirect(w, r, newPath, http.StatusTemporaryRedirect)
	})
}

func (h *Knot) configureOwner(ctx context.Context) error {
	cfgOwner := h.c.Server.Owner

	rbacDomain := "thisserver"

	existing, err := h.e.GetKnotUsersByRole("server:owner", rbacDomain)
	if err != nil {
		return err
	}

	switch len(existing) {
	case 0:
		// no owner configured, continue
	case 1:
		// find existing owner
		existingOwner := existing[0]

		// no ownership change, this is okay
		if existingOwner == h.c.Server.Owner {
			break
		}

		// remove existing owner
		if err = db.RemoveDid(h.db, existingOwner); err != nil {
			return err
		}
		if err = h.e.RemoveKnotOwner(rbacDomain, existingOwner); err != nil {
			return err
		}

	default:
		return fmt.Errorf("more than one owner in DB, try deleting %q and starting over", h.c.Server.DBPath)
	}

	if err = db.AddDid(h.db, cfgOwner); err != nil {
		return fmt.Errorf("failed to add owner to DB: %w", err)
	}
	if err := h.e.AddKnotOwner(rbacDomain, cfgOwner); err != nil {
		return fmt.Errorf("failed to add owner to RBAC: %w", err)
	}

	err = keys.FetchAndStore(ctx, h.resolver.Directory(), h.db, syntax.DID(cfgOwner))
	if err != nil {
		h.l.Error("fetching and adding owners public keys", "error", err, "did", cfgOwner)
	}

	return nil
}
