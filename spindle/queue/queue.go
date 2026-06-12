package queue

import (
	"slices"
	"sync"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type Job struct {
	Run    func() error
	OnFail func(error)
}

type ownedJob struct {
	owner syntax.DID
	job   Job
}

// prefers users with fewer running jobs, otherwise it's FIFO
type Queue struct {
	mu      sync.Mutex
	cond    *sync.Cond
	queue   []ownedJob
	running map[syntax.DID]int
	maxSize int
	workers int
	stopped bool
	wg      sync.WaitGroup
}

func NewQueue(queueSize, numWorkers int) *Queue {
	q := &Queue{
		maxSize: queueSize,
		workers: numWorkers,
		running: make(map[syntax.DID]int),
	}
	q.cond = sync.NewCond(&q.mu)
	return q
}

// todo(dawn): add a per-user cap so a single user can't fill the queue
func (q *Queue) Enqueue(owner syntax.DID, job Job) bool {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.stopped || len(q.queue) >= q.maxSize {
		return false
	}
	q.queue = append(q.queue, ownedJob{owner: owner, job: job})
	q.cond.Signal()
	return true
}

func (q *Queue) Start() {
	for range q.workers {
		q.wg.Add(1)
		go q.worker()
	}
}

func (q *Queue) worker() {
	defer q.wg.Done()
	for {
		picked, ok := q.takeNext()
		if !ok {
			return
		}

		err := picked.job.Run()
		if err != nil && picked.job.OnFail != nil {
			picked.job.OnFail(err)
		}

		q.finish(picked.owner)
	}
}

// get or wait for the next job
func (q *Queue) takeNext() (ownedJob, bool) {
	q.mu.Lock()
	defer q.mu.Unlock()

	for len(q.queue) == 0 && !q.stopped {
		q.cond.Wait() // waiting for jobs
	}
	if q.stopped && len(q.queue) == 0 {
		return ownedJob{}, false // no jobs are left and the queue is stopped
	}

	idx := q.pickBest()
	picked := q.queue[idx]
	q.queue = slices.Delete(q.queue, idx, idx+1)
	q.running[picked.owner]++

	return picked, true
}

// index of the queued job whose owner has the fewest currently-running jobs,
// tiebreaking by arrival order.
func (q *Queue) pickBest() int {
	best := 0
	for idx, job := range q.queue {
		if q.running[job.owner] < q.running[q.queue[best].owner] {
			best = idx
		}
	}
	return best
}

// called when finishing a job
func (q *Queue) finish(owner syntax.DID) {
	q.mu.Lock()
	defer q.mu.Unlock()

	q.running[owner]--
	if q.running[owner] <= 0 {
		delete(q.running, owner)
	}
}

func (q *Queue) Stop() {
	q.mu.Lock()
	q.stopped = true
	q.cond.Broadcast()
	q.mu.Unlock()
	q.wg.Wait()
}
