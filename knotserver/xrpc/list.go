package xrpc

import (
	"fmt"
	"net/http"
	"strconv"

	"tangled.org/core/knotserver/db"
)

func parseListParams(r *http.Request) (db.ListPage, error) {
	q := r.URL.Query()
	p := db.ListPage{Limit: db.ListDefaultLimit, Desc: true}

	if s := q.Get("limit"); s != "" {
		n, err := strconv.Atoi(s)
		if err != nil {
			return p, fmt.Errorf("limit must be an integer")
		}
		p.Limit = min(max(n, 1), db.ListMaxLimit)
	}

	if s := q.Get("cursor"); s != "" {
		n, err := strconv.Atoi(s)
		if err != nil {
			return p, fmt.Errorf("cursor must be an integer")
		}
		p.Cursor = &n
	}

	switch q.Get("order") {
	case "", "desc":
		p.Desc = true
	case "asc":
		p.Desc = false
	default:
		return p, fmt.Errorf("order must be 'asc' or 'desc'")
	}

	return p, nil
}

func mapSlice[T, U any](items []T, f func(T) U) []U {
	out := make([]U, len(items))
	for i, it := range items {
		out[i] = f(it)
	}
	return out
}
