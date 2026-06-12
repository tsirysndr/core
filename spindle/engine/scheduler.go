package engine

import (
	"context"
	"fmt"
	"slices"
	"sync"
	"time"
)

const defaultAgingThreshold = 30 * time.Second

type Resources[Self any] interface {
	Fits(Self) bool
	Add(Self) Self
	Sub(Self) Self
}

type ResourceScheduler[R Resources[R]] struct {
	mu             sync.Mutex
	budget         R
	max            R
	used           R
	queue          []*resourceWaiter[R]
	now            func() time.Time // get time now, is a field for mocking
	agingThreshold time.Duration
}

type resourceWaiter[R Resources[R]] struct {
	req        R
	ready      chan struct{}
	enqueuedAt time.Time
}

type resourceLease[R Resources[R]] struct {
	scheduler *ResourceScheduler[R]
	req       R
	once      sync.Once
}

func NewResourceScheduler[R Resources[R]](budget, max R, agingThreshold time.Duration) *ResourceScheduler[R] {
	if agingThreshold <= 0 {
		agingThreshold = defaultAgingThreshold
	}
	return &ResourceScheduler[R]{
		budget:         budget,
		max:            max,
		now:            time.Now,
		agingThreshold: agingThreshold,
	}
}

func (s *ResourceScheduler[R]) Acquire(ctx context.Context, req R) (WorkflowSlot, error) {
	if s == nil {
		return NoopSlot{}, nil
	}

	s.mu.Lock()
	if !req.Fits(s.budget) || !req.Fits(s.max) {
		s.mu.Unlock()
		return nil, fmt.Errorf("%w: request=%v budget=%v max=%v", ErrNoWorkflowSlots, req, s.budget, s.max)
	}
	if len(s.queue) == 0 && s.used.Add(req).Fits(s.budget) {
		s.used = s.used.Add(req)
		s.mu.Unlock()
		return &resourceLease[R]{scheduler: s, req: req}, nil
	}

	waiter := &resourceWaiter[R]{req: req, ready: make(chan struct{}), enqueuedAt: s.now()}
	s.queue = append(s.queue, waiter)
	s.schedule()
	s.mu.Unlock()

	select {
	case <-waiter.ready:
		return &resourceLease[R]{scheduler: s, req: req}, nil
	case <-ctx.Done():
		s.mu.Lock()
		select {
		case <-waiter.ready:
			// undo committed resources, schedule already did that
			s.used = s.used.Sub(req)
		default:
			// still in queue, just remove
			s.remove(waiter)
		}
		s.schedule()
		s.mu.Unlock()
		return nil, ctx.Err()
	}
}

func (l *resourceLease[R]) Release() {
	if l == nil || l.scheduler == nil {
		return
	}
	l.once.Do(func() {
		l.scheduler.release(l.req)
	})
}

func (s *ResourceScheduler[R]) release(req R) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.used = s.used.Sub(req)
	s.schedule()
}

// start every waiter whose request fits. once a waiter is older than
// agingThreshold, count its request as already used so younger waiters
// stop being scheduled ahead of it.
func (s *ResourceScheduler[R]) schedule() {
	var reserved R
	now := s.now()
	i := 0
	for i < len(s.queue) {
		w := s.queue[i]
		if s.used.Add(reserved).Add(w.req).Fits(s.budget) {
			s.queue = slices.Delete(s.queue, i, i+1)
			s.used = s.used.Add(w.req)
			close(w.ready)
			continue
		}
		if now.Sub(w.enqueuedAt) >= s.agingThreshold {
			reserved = reserved.Add(w.req)
		}
		i++
	}
}

func (s *ResourceScheduler[R]) remove(waiter *resourceWaiter[R]) {
	for i, candidate := range s.queue {
		if candidate != waiter {
			continue
		}
		s.queue = slices.Delete(s.queue, i, i+1)
		return
	}
}
