package db

import (
	"context"
	"database/sql"
	"errors"
)

func MarkKnotAclNative(ctx context.Context, e Execer, domain string) error {
	_, err := e.ExecContext(
		ctx,
		`insert into knot_acl_native (domain) values (?) on conflict (domain) do nothing`,
		domain,
	)
	return err
}

func IsKnotAclNative(ctx context.Context, e Execer, domain string) (bool, error) {
	var one int
	err := e.QueryRowContext(ctx, `select 1 from knot_acl_native where domain = ?`, domain).Scan(&one)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	return true, nil
}
