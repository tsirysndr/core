package db

import (
	"database/sql"
	"fmt"
)

const (
	ListDefaultLimit = 50
	ListMaxLimit     = 1000
)

type ListPage struct {
	Limit  int
	Cursor *int
	Desc   bool
}

func (p ListPage) limit() int {
	switch {
	case p.Limit <= 0:
		return ListDefaultLimit
	case p.Limit > ListMaxLimit:
		return ListMaxLimit
	default:
		return p.Limit
	}
}

func (p ListPage) clause() (string, []any) {
	dir, cmp := "asc", ">"
	if p.Desc {
		dir, cmp = "desc", "<"
	}
	if p.Cursor != nil {
		return fmt.Sprintf("where id %s ? order by id %s limit ?", cmp, dir), []any{*p.Cursor, p.limit() + 1}
	}
	return fmt.Sprintf("order by id %s limit ?", dir), []any{p.limit() + 1}
}

func listPaged[T any](
	q DBTX,
	query string,
	args []any,
	p ListPage,
	scan func(*sql.Rows) (T, error),
	idOf func(T) int,
) ([]T, *int, error) {
	clause, pArgs := p.clause()
	rows, err := q.Query("select * from ("+query+") "+clause, append(args, pArgs...)...)
	if err != nil {
		return nil, nil, err
	}
	defer rows.Close()

	out := []T{}
	for rows.Next() {
		v, err := scan(rows)
		if err != nil {
			return nil, nil, err
		}
		out = append(out, v)
	}
	if err := rows.Err(); err != nil {
		return nil, nil, err
	}

	if limit := p.limit(); len(out) > limit {
		out = out[:limit]
		last := idOf(out[len(out)-1])
		return out, &last, nil
	}
	return out, nil, nil
}
