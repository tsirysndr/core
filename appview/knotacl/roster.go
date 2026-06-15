package knotacl

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"sync"
	"sync/atomic"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"golang.org/x/sync/singleflight"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

const (
	reconcileTTL     = 5 * time.Minute
	reconcileBackoff = 15 * time.Second
	cursorRetention  = 1 * time.Hour
)

var errReconcileBackoff = errors.New("reconcile suppressed during backoff")

type Cursor int64

type scopeState struct {
	mu       sync.Mutex
	gen      atomic.Uint64
	failedAt time.Time
	refs     int
}

type roster struct {
	store *db.DB
	src   lister
	ttl   time.Duration
	now   func() time.Time
	log   *slog.Logger
	group singleflight.Group

	mu     sync.Mutex
	scopes map[string]*scopeState
}

func newRoster(store *db.DB, src lister, ttl time.Duration, now func() time.Time, logger *slog.Logger) *roster {
	if now == nil {
		now = time.Now
	}
	if logger == nil {
		logger = slog.Default()
	}
	return &roster{
		store:  store,
		src:    src,
		ttl:    ttl,
		now:    now,
		log:    logger,
		scopes: map[string]*scopeState{},
	}
}

func (r *roster) GetKnotMembers(ctx context.Context, host string) ([]string, error) {
	return r.serve(ctx, memberScope(host),
		func() error { return r.reconcileMembers(ctx, host) },
		func() ([]string, error) {
			rows, err := db.GetKnotMembers(r.store, orm.FilterEq("domain", host))
			if err != nil {
				return nil, err
			}
			return mapSlice(rows, func(km models.KnotMember) string { return km.Subject.String() }), nil
		},
	)
}

func (r *roster) GetRepoCollaborators(ctx context.Context, host, repoDid string) ([]string, error) {
	return r.serve(ctx, collabScope(repoDid),
		func() error { return r.reconcileCollaborators(ctx, host, repoDid) },
		func() ([]string, error) {
			rows, err := db.GetCollaborators(r.store, orm.FilterEq("repo_did", repoDid))
			if err != nil {
				return nil, err
			}
			return mapSlice(rows, func(c models.Collaborator) string { return c.SubjectDid.String() }), nil
		},
	)
}

func (r *roster) AddKnotMember(host string, subject syntax.DID, cursor Cursor) error {
	return r.applyDelta(memberScope(host), subject, cursor, func(tx *sql.Tx) error {
		if err := db.RemoveKnotMember(tx,
			orm.FilterEq("domain", host),
			orm.FilterEq("subject", subject.String()),
		); err != nil {
			return err
		}
		return db.AddKnotMember(tx, models.KnotMember{Domain: host, Subject: subject})
	})
}

func (r *roster) RemoveKnotMember(host string, subject syntax.DID, cursor Cursor) error {
	return r.applyDelta(memberScope(host), subject, cursor, func(tx *sql.Tx) error {
		return db.RemoveKnotMember(tx,
			orm.FilterEq("domain", host),
			orm.FilterEq("subject", subject.String()),
		)
	})
}

// NOTE: maybe TODO or not, but no Did of adder means no ability to suggest a vouch for the person they just added as collaborator.
func (r *roster) AddCollaborator(repoDid, subject syntax.DID, cursor Cursor) error {
	return r.applyDelta(collabScope(repoDid.String()), subject, cursor, func(tx *sql.Tx) error {
		return db.AddCollaborator(tx, models.Collaborator{SubjectDid: subject, RepoDid: repoDid})
	})
}

func (r *roster) RemoveCollaborator(repoDid, subject syntax.DID, cursor Cursor) error {
	return r.applyDelta(collabScope(repoDid.String()), subject, cursor, func(tx *sql.Tx) error {
		return db.DeleteCollaborator(tx,
			orm.FilterEq("repo_did", repoDid.String()),
			orm.FilterEq("subject_did", subject.String()),
		)
	})
}

func (r *roster) applyDelta(scope string, subject syntax.DID, cursor Cursor, mutate func(*sql.Tx) error) error {
	st := r.acquire(scope)
	defer r.release(scope, st)

	st.mu.Lock()
	defer st.mu.Unlock()

	tx, err := r.store.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	seen, ok, err := seenCursor(tx, scope, subject)
	if err != nil {
		return err
	}
	if ok && cursor <= seen {
		return nil
	}

	if err := mutate(tx); err != nil {
		return err
	}
	if err := recordCursor(tx, scope, subject, cursor); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}

	st.gen.Add(1)
	return nil
}

func (r *roster) InvalidateMembers(host string) {
	_ = clearSyncedAt(r.store, memberScope(host))
}

func (r *roster) InvalidateCollaborators(host, repoDid string) {
	_ = clearSyncedAt(r.store, collabScope(repoDid))
}

func (r *roster) serve(ctx context.Context, key string, reconcile func() error, read func() ([]string, error)) ([]string, error) {
	if memo := memoFrom(ctx); memo != nil {
		if v, ok := memo.get(key); ok {
			return slices.Clone(v), nil
		}
	}

	recErr := r.maybeReconcile(key, reconcile)

	subjects, err := read()
	if err != nil {
		return nil, err
	}

	if len(subjects) == 0 && recErr != nil && !r.everSynced(key) {
		return nil, fmt.Errorf("%w: %v", ErrKnotUnreachable, recErr)
	}

	subjects = dedup(subjects)
	if memo := memoFrom(ctx); memo != nil {
		memo.put(key, subjects)
	}
	return slices.Clone(subjects), nil
}

func (r *roster) maybeReconcile(key string, reconcile func() error) error {
	if r.fresh(key) {
		return nil
	}
	if r.backingOff(key) {
		return errReconcileBackoff
	}
	_, err, _ := r.group.Do(key, func() (any, error) {
		if r.fresh(key) {
			return nil, nil
		}
		if r.backingOff(key) {
			return nil, errReconcileBackoff
		}
		return nil, reconcile()
	})
	return err
}

func (r *roster) reconcileMembers(ctx context.Context, host string) error {
	scope := memberScope(host)
	st := r.acquire(scope)
	defer r.release(scope, st)

	genBefore := st.gen.Load()
	subjects, err := r.src.GetKnotMembers(ctx, host)
	if err != nil {
		r.markFailed(st)
		return err
	}
	return r.commitReconcile(st, scope, genBefore, func(tx *sql.Tx) error {
		if err := db.RemoveKnotMember(tx, orm.FilterEq("domain", host)); err != nil {
			return err
		}
		for _, s := range subjects {
			did, perr := syntax.ParseDID(s)
			if perr != nil {
				r.log.Warn("dropping malformed member DID from reconcile", "host", host, "subject", s, "error", perr)
				continue
			}
			if err := db.AddKnotMember(tx, models.KnotMember{Domain: host, Subject: did}); err != nil {
				return err
			}
		}
		return nil
	})
}

func (r *roster) reconcileCollaborators(ctx context.Context, host, repoDid string) error {
	repo, perr := syntax.ParseDID(repoDid)
	if perr != nil {
		return perr
	}
	scope := collabScope(repoDid)
	st := r.acquire(scope)
	defer r.release(scope, st)

	genBefore := st.gen.Load()
	subjects, err := r.src.GetRepoCollaborators(ctx, host, repoDid)
	if err != nil {
		r.markFailed(st)
		return err
	}
	return r.commitReconcile(st, scope, genBefore, func(tx *sql.Tx) error {
		if err := db.DeleteCollaborator(tx, orm.FilterEq("repo_did", repoDid)); err != nil {
			return err
		}
		for _, s := range subjects {
			did, perr := syntax.ParseDID(s)
			if perr != nil {
				r.log.Warn("dropping malformed collaborator DID from reconcile", "repo_did", repoDid, "subject", s, "error", perr)
				continue
			}
			if err := db.AddCollaborator(tx, models.Collaborator{SubjectDid: did, RepoDid: repo}); err != nil {
				return err
			}
		}
		return nil
	})
}

func (r *roster) commitReconcile(st *scopeState, scope string, genBefore uint64, replace func(*sql.Tx) error) error {
	st.mu.Lock()
	defer st.mu.Unlock()

	if st.gen.Load() != genBefore {
		if err := setSyncedAt(r.store, scope, r.now()); err != nil {
			return err
		}
		r.clearFailed(st)
		return nil
	}

	tx, err := r.store.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := replace(tx); err != nil {
		return err
	}
	if err := setSyncedAt(tx, scope, r.now()); err != nil {
		return err
	}
	if err := pruneCursors(tx, scope, int64(cursorRetention)); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	r.clearFailed(st)
	return nil
}

func (r *roster) acquire(scope string) *scopeState {
	r.mu.Lock()
	defer r.mu.Unlock()
	st := r.scopes[scope]
	if st == nil {
		st = &scopeState{}
		r.scopes[scope] = st
	}
	st.refs++
	return st
}

func (r *roster) release(scope string, st *scopeState) {
	r.mu.Lock()
	defer r.mu.Unlock()
	st.refs--
	if st.refs == 0 && st.failedAt.IsZero() {
		delete(r.scopes, scope)
	}
}

func (r *roster) markFailed(st *scopeState) {
	r.mu.Lock()
	defer r.mu.Unlock()
	st.failedAt = r.now()
}

func (r *roster) clearFailed(st *scopeState) {
	r.mu.Lock()
	defer r.mu.Unlock()
	st.failedAt = time.Time{}
}

func (r *roster) backingOff(scope string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	st, ok := r.scopes[scope]
	if !ok {
		return false
	}
	return !st.failedAt.IsZero() && r.now().Sub(st.failedAt) < reconcileBackoff
}

func (r *roster) fresh(key string) bool {
	at, ok, err := getSyncedAt(r.store, key)
	if err != nil || !ok {
		return false
	}
	return r.now().Sub(at) < r.ttl
}

func (r *roster) everSynced(key string) bool {
	_, ok, _ := getSyncedAt(r.store, key)
	return ok
}

func memberScope(host string) string { return "m\x00" + host }

func collabScope(repoDid string) string { return "c\x00" + repoDid }

func getSyncedAt(e db.Execer, key string) (time.Time, bool, error) {
	var raw string
	err := e.QueryRow(`select synced_at from knotacl_sync where scope_key = ?`, key).Scan(&raw)
	if errors.Is(err, sql.ErrNoRows) {
		return time.Time{}, false, nil
	}
	if err != nil {
		return time.Time{}, false, err
	}
	at, err := time.Parse(time.RFC3339, raw)
	if err != nil {
		return time.Time{}, false, err
	}
	return at, true, nil
}

func setSyncedAt(e db.Execer, key string, at time.Time) error {
	_, err := e.Exec(
		`insert into knotacl_sync (scope_key, synced_at) values (?, ?)
		 on conflict(scope_key) do update set synced_at = excluded.synced_at`,
		key, at.UTC().Format(time.RFC3339),
	)
	return err
}

func clearSyncedAt(e db.Execer, key string) error {
	_, err := e.Exec(`delete from knotacl_sync where scope_key = ?`, key)
	return err
}

func seenCursor(e db.Execer, scope string, subject syntax.DID) (Cursor, bool, error) {
	var raw int64
	err := e.QueryRow(
		`select cursor from knotacl_delta_cursor where scope_key = ? and subject = ?`,
		scope, subject.String(),
	).Scan(&raw)
	if errors.Is(err, sql.ErrNoRows) {
		return 0, false, nil
	}
	if err != nil {
		return 0, false, err
	}
	return Cursor(raw), true, nil
}

func recordCursor(e db.Execer, scope string, subject syntax.DID, cursor Cursor) error {
	_, err := e.Exec(
		`insert into knotacl_delta_cursor (scope_key, subject, cursor) values (?, ?, ?)
		 on conflict(scope_key, subject) do update set cursor = excluded.cursor`,
		scope, subject.String(), int64(cursor),
	)
	return err
}

func pruneCursors(e db.Execer, scope string, retentionNanos int64) error {
	_, err := e.Exec(
		`delete from knotacl_delta_cursor
		 where scope_key = ?
		   and cursor < (select max(cursor) from knotacl_delta_cursor where scope_key = ?) - ?`,
		scope, scope, retentionNanos,
	)
	return err
}
