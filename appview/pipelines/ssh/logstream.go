package ssh

import (
	"context"
	"time"

	"tangled.org/core/api/tangled"
)

type step struct {
	id        int64
	name      string
	command   string
	lines     []string
	startTime time.Time
	endTime   time.Time
	finished  bool
}

type logDoneMsg struct {
	err error
}

type logEventMsg struct {
	ev     *tangled.CiSubscribePipelineLogs_Event
	events chan *tangled.CiSubscribePipelineLogs_Event
	done   chan error
}

type eventScheduler struct {
	ch chan *tangled.CiSubscribePipelineLogs_Event
}

func newEventScheduler() *eventScheduler {
	return &eventScheduler{ch: make(chan *tangled.CiSubscribePipelineLogs_Event, 1024)}
}

func (s *eventScheduler) AddWork(ctx context.Context, _ string, v *tangled.CiSubscribePipelineLogs_Event) error {
	select {
	case s.ch <- v:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (s *eventScheduler) Shutdown() { close(s.ch) }
