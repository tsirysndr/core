package knotstream

import "tangled.org/core/knotmirror/models"

// subscription represents websocket connection with that host
type subscription struct {
	hostname string

	// embedded parallel job scheduler
	scheduler *ParallelScheduler
}

func (s *subscription) LastSeq() int64 {
	return s.scheduler.LastSeq()
}

func (s *subscription) HostCursor() models.HostCursor {
	return models.HostCursor{
		Hostname: s.hostname,
		LastSeq:  s.LastSeq(),
	}
}
