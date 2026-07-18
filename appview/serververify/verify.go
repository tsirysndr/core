package serververify

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/netutil"
	"tangled.org/core/orm"
	"tangled.org/core/rbac"
	"tangled.org/core/xrpc/xrpcclient"
)

var (
	FetchError = errors.New("failed to fetch owner")
)

const verifyTimeout = 10 * time.Second

// fetchOwner fetches the owner DID from a server's /owner endpoint
func fetchOwner(ctx context.Context, domain string, dev bool) (string, error) {
	scheme := "https"
	if dev {
		scheme = "http"
	}

	host := fmt.Sprintf("%s://%s", scheme, domain)
	dialer := netutil.SSRFDialer(dev)
	dialer.Timeout = 5 * time.Second
	dialer.KeepAlive = 30 * time.Second

	transport := &http.Transport{
		DialContext: dialer.DialContext,
	}
	xrpcc := &indigoxrpc.Client{
		Host: host,
		Client: &http.Client{
			Timeout:   verifyTimeout,
			Transport: transport,
		},
	}

	res, err := tangled.Owner(ctx, xrpcc)
	if err != nil {
		var xrpcerr *indigoxrpc.Error
		if !errors.As(err, &xrpcerr) {
			return "", err
		}
		if handled := xrpcclient.HandleXrpcErr(err); handled != nil {
			return "", handled
		}
	}

	return res.Owner, nil
}

type OwnerMismatch struct {
	expected string
	observed string
}

func (e *OwnerMismatch) Error() string {
	return fmt.Sprintf("owner mismatch: %q != %q", e.expected, e.observed)
}

// RunVerification verifies that the server at the given domain has the expected owner
func RunVerification(ctx context.Context, domain, expectedOwner string, dev bool) error {
	observedOwner, err := fetchOwner(ctx, domain, dev)
	if err != nil {
		return err
	}

	if observedOwner != expectedOwner {
		return &OwnerMismatch{
			expected: expectedOwner,
			observed: observedOwner,
		}
	}

	return nil
}

// MarkSpindleVerified marks a spindle as verified in the DB and adds the user as its owner
func MarkSpindleVerified(d *db.DB, e *rbac.Enforcer, instance, owner string) (int64, error) {
	tx, err := d.Begin()
	if err != nil {
		return 0, fmt.Errorf("failed to create txn: %w", err)
	}
	committed := false
	defer func() {
		if committed {
			return
		}
		tx.Rollback()
		e.E.LoadPolicy()
	}()

	// mark this spindle as verified in the db
	rowId, err := db.VerifySpindle(
		tx,
		orm.FilterEq("owner", owner),
		orm.FilterEq("instance", instance),
	)
	if err != nil {
		return 0, fmt.Errorf("failed to write to DB: %w", err)
	}

	err = e.AddSpindleOwner(instance, owner)
	if err != nil {
		return 0, fmt.Errorf("failed to update ACL: %w", err)
	}

	err = tx.Commit()
	if err != nil {
		return 0, fmt.Errorf("failed to commit txn: %w", err)
	}

	err = e.E.SavePolicy()
	if err != nil {
		return 0, fmt.Errorf("failed to update ACL: %w", err)
	}
	committed = true

	return rowId, nil
}

// MarkKnotVerified marks a knot as verified and sets up ownership/permissions
func MarkKnotVerified(d *db.DB, e *rbac.Enforcer, domain, owner string) error {
	tx, err := d.BeginTx(context.Background(), nil)
	if err != nil {
		return fmt.Errorf("failed to start tx: %w", err)
	}
	committed := false
	defer func() {
		if committed {
			return
		}
		tx.Rollback()
		e.E.LoadPolicy()
	}()

	// mark as registered
	err = db.MarkRegistered(
		tx,
		orm.FilterEq("did", owner),
		orm.FilterEq("domain", domain),
	)
	if err != nil {
		return fmt.Errorf("failed to register domain: %w", err)
	}

	// add basic acls for this domain
	err = e.AddKnot(domain)
	if err != nil {
		return fmt.Errorf("failed to add knot to enforcer: %w", err)
	}

	// add this did as owner of this domain
	err = e.AddKnotOwner(domain, owner)
	if err != nil {
		return fmt.Errorf("failed to add knot owner to enforcer: %w", err)
	}

	err = tx.Commit()
	if err != nil {
		return fmt.Errorf("failed to commit changes: %w", err)
	}

	err = e.E.SavePolicy()
	if err != nil {
		return fmt.Errorf("failed to update ACLs: %w", err)
	}
	committed = true

	return nil
}
