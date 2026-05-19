package rbac

import (
	"database/sql"
	"log/slog"
	"slices"
)

type Txn struct {
	SQLTx     *sql.Tx
	undos     []func() error
	committed bool
}

func NewTxn(sqlTx *sql.Tx) *Txn {
	return &Txn{SQLTx: sqlTx}
}

func (t *Txn) AddUndo(undo func() error) {
	t.undos = append(t.undos, undo)
}

func (t *Txn) Commit() error {
	if err := t.SQLTx.Commit(); err != nil {
		return err
	}
	t.committed = true
	return nil
}

func (t *Txn) Cleanup(l *slog.Logger) {
	if t.committed {
		return
	}
	t.SQLTx.Rollback()
	for _, undo := range slices.Backward(t.undos) {
		if err := undo(); err != nil {
			l.Error("failed to reverse ACL change", "err", err)
		}
	}
}
