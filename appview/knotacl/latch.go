package knotacl

import (
	"context"
	"log/slog"
	"time"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotcompat"
)

const latchOpTimeout = 5 * time.Second

type latch struct {
	execer db.Execer
	log    *slog.Logger
}

func NewLatch(execer db.Execer, logger *slog.Logger) knotcompat.NativeLatch {
	return latch{execer: execer, log: logger}
}

func (l latch) IsNative(host string) bool {
	ctx, cancel := context.WithTimeout(context.Background(), latchOpTimeout)
	defer cancel()
	native, err := db.IsKnotAclNative(ctx, l.execer, host)
	return err == nil && native
}

func (l latch) MarkNative(host string) {
	ctx, cancel := context.WithTimeout(context.Background(), latchOpTimeout)
	defer cancel()
	if err := db.MarkKnotAclNative(ctx, l.execer, host); err != nil {
		l.log.Error("failed to persist native knot latch, it will be re-probed after restart", "host", host, "err", err)
	}
}
