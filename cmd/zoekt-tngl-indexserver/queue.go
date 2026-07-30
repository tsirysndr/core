package main

import (
	"sync"

	"tangled.org/core/repoident"
)

// deduplicating index work queue
type Queue struct {
	mu      sync.Mutex
	order   []repoident.RepoDid
	pending map[repoident.RepoDid]indexRequest
	size    int
}

func NewQueue(size int) *Queue {
	return &Queue{
		pending: make(map[repoident.RepoDid]indexRequest),
		size:    size,
	}
}

func (q *Queue) Enqueue(req indexRequest) bool {
	q.mu.Lock()
	defer q.mu.Unlock()

	if _, exists := q.pending[req.Repo]; exists {
		q.pending[req.Repo] = req // replace payload, keep position
		return true
	}

	if len(q.order) >= q.size {
		return false // queue full
	}

	q.order = append(q.order, req.Repo)
	q.pending[req.Repo] = req
	return true
}

func (q *Queue) Pop() (indexRequest, bool) {
	q.mu.Lock()
	defer q.mu.Unlock()

	if len(q.order) == 0 {
		return indexRequest{}, false
	}

	did := q.order[0]
	q.order = q.order[1:]
	if len(q.order) == 0 {
		q.order = nil // release the backing array
	}

	req, ok := q.pending[did]
	delete(q.pending, did)
	return req, ok
}

func (q *Queue) Snapshot() []indexRequest {
	q.mu.Lock()
	defer q.mu.Unlock()

	out := make([]indexRequest, 0, len(q.order))
	for _, did := range q.order {
		if req, ok := q.pending[did]; ok {
			out = append(out, req)
		}
	}
	return out
}
