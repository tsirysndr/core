package knotstream

import (
	"context"
	"log/slog"
	"sync"
	"sync/atomic"
	"time"

	"tangled.org/core/log"
)

type ParallelScheduler struct {
	concurrency int

	do func(ctx context.Context, task *Task) error

	feeder    chan *Task
	lk        sync.Mutex
	scheduled map[string][]*Task
	lastSeq   atomic.Int64

	logger *slog.Logger
}

type Task struct {
	Key     string
	message []byte
}

func NewParallelScheduler(maxC int, ident string, do func(context.Context, *Task) error) *ParallelScheduler {
	return &ParallelScheduler{
		concurrency: maxC,
		do:          do,
		feeder:      make(chan *Task),
		scheduled:   make(map[string][]*Task),
		logger:      log.New("parallel-scheduler"),
	}
}

func (s *ParallelScheduler) Start(ctx context.Context) {
	for range s.concurrency {
		go s.ForEach(ctx, s.do)
	}
}

func (s *ParallelScheduler) AddTask(ctx context.Context, task *Task) {
	s.lk.Lock()
	if st, ok := s.scheduled[task.Key]; ok {
		// schedule task
		s.scheduled[task.Key] = append(st, task)
		s.lk.Unlock()
		return
	}
	s.scheduled[task.Key] = []*Task{}
	s.lk.Unlock()

	select {
	case <-ctx.Done():
		return
	case s.feeder <- task:
		return
	}
}

func (s *ParallelScheduler) ForEach(ctx context.Context, fn func(context.Context, *Task) error) {
	for task := range s.feeder {
		for task != nil {
			select {
			case <-ctx.Done():
				return
			default:
			}
			if err := fn(ctx, task); err != nil {
				s.logger.Error("event handler failed", "err", err)
			}

			s.lk.Lock()
			func() {
				rem, ok := s.scheduled[task.Key]
				if !ok {
					s.logger.Error("should always have an 'active' entry if a worker is processing a job")
				}
				if len(rem) == 0 {
					delete(s.scheduled, task.Key)
					task = nil
				} else {
					task = rem[0]
					s.scheduled[task.Key] = rem[1:]
				}

				// TODO: update seq from received message
				s.lastSeq.Store(time.Now().UnixNano())
			}()
			s.lk.Unlock()
		}
	}
}

func (s *ParallelScheduler) LastSeq() int64 {
	return s.lastSeq.Load()
}
