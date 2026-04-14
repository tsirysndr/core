package orm

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"reflect"
	"strings"

	"github.com/mattn/go-sqlite3"
)

func IsUniqueViolation(err error) bool {
	var sqlErr sqlite3.Error
	if !errors.As(err, &sqlErr) {
		return false
	}
	return sqlErr.ExtendedCode == sqlite3.ErrConstraintUnique ||
		sqlErr.ExtendedCode == sqlite3.ErrConstraintPrimaryKey
}

type migrationFn = func(*sql.Tx) error

func RunMigration(c *sql.Conn, logger *slog.Logger, name string, migrationFn migrationFn) error {
	logger = logger.With("migration", name)

	tx, err := c.BeginTx(context.Background(), nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()

	var exists bool
	err = tx.QueryRow("select exists (select 1 from migrations where name = ?)", name).Scan(&exists)
	if err != nil {
		return err
	}

	if !exists {
		// run migration
		err = migrationFn(tx)
		if err != nil {
			logger.Error("failed to run migration", "err", err)
			return err
		}

		// mark migration as complete
		_, err = tx.Exec("insert into migrations (name) values (?)", name)
		if err != nil {
			logger.Error("failed to mark migration as complete", "err", err)
			return err
		}

		// commit the transaction
		if err := tx.Commit(); err != nil {
			return err
		}

		logger.Info("migration applied successfully")
	} else {
		logger.Warn("skipped migration, already applied")
	}

	return nil
}

type Filter struct {
	Key string
	arg any
	Cmp string
}

func newFilter(key, cmp string, arg any) Filter {
	return Filter{
		Key: key,
		arg: arg,
		Cmp: cmp,
	}
}

func FilterEq(key string, arg any) Filter      { return newFilter(key, "=", arg) }
func FilterNotEq(key string, arg any) Filter   { return newFilter(key, "<>", arg) }
func FilterGte(key string, arg any) Filter     { return newFilter(key, ">=", arg) }
func FilterLte(key string, arg any) Filter     { return newFilter(key, "<=", arg) }
func FilterIs(key string, arg any) Filter      { return newFilter(key, "is", arg) }
func FilterIsNot(key string, arg any) Filter   { return newFilter(key, "is not", arg) }
func FilterIn(key string, arg any) Filter      { return newFilter(key, "in", arg) }
func FilterLike(key string, arg any) Filter    { return newFilter(key, "like", arg) }
func FilterNotLike(key string, arg any) Filter { return newFilter(key, "not like", arg) }
func FilterContains(key string, arg any) Filter {
	return newFilter(key, "like", fmt.Sprintf("%%%v%%", arg))
}

func (f Filter) Condition() string {
	rv := reflect.ValueOf(f.arg)
	kind := rv.Kind()

	// if we have `FilterIn(k, [1, 2, 3])`, compile it down to `k in (?, ?, ?)`
	if (kind == reflect.Slice && rv.Type().Elem().Kind() != reflect.Uint8) || kind == reflect.Array {
		if rv.Len() == 0 {
			// always false
			return "1 = 0"
		}

		placeholders := make([]string, rv.Len())
		for i := range placeholders {
			placeholders[i] = "?"
		}

		return fmt.Sprintf("%s %s (%s)", f.Key, f.Cmp, strings.Join(placeholders, ", "))
	}

	return fmt.Sprintf("%s %s ?", f.Key, f.Cmp)
}

func (f Filter) Arg() []any {
	rv := reflect.ValueOf(f.arg)
	kind := rv.Kind()
	if (kind == reflect.Slice && rv.Type().Elem().Kind() != reflect.Uint8) || kind == reflect.Array {
		if rv.Len() == 0 {
			return nil
		}

		out := make([]any, rv.Len())
		for i := range rv.Len() {
			out[i] = rv.Index(i).Interface()
		}
		return out
	}

	return []any{f.arg}
}
