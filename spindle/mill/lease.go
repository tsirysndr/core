package mill

import (
	"sync"

	"tangled.org/core/spindle/models"

	millv1 "tangled.org/core/spindle/mill/proto/gen"
)

// the mill's view of a remote attempt
type leaseState int32

const (
	// won a bid, executor is holding a seat, not yet committed
	leaseReserved leaseState = iota
	// the mill sent CommitLease, the executor may already be running, but
	// the mill may not have seen Committed yet
	leaseCommitting
	// the executor acked CommitLease, job is running on the executor
	leaseRunning
	// terminal result arrived or we gave up, no further action
	leaseDone
)

type cancelAction int

const (
	cancelNoop cancelAction = iota
	cancelLocal
	cancelRemote
)

// mill-side fencing token for one placed job
type RemoteLease struct {
	id     string
	nodeID string
	epoch  string
	engine string
	wid    models.WorkflowId // job this lease carries, set once placed
	// restored after a mill restart. no RunStep waits on it, so terminals
	// and death are authored directly. set before publication, never mutated
	orphaned     bool
	claimed      bool
	cancelAcked  bool
	cleanedUp    bool
	cleanupRetry bool

	mu       sync.Mutex
	state    leaseState
	cancel   bool
	reason   string
	released bool
	terminal chan *millv1.AttemptResult // buffered(1), RunStep waits here

	finishMu sync.Mutex
}

func newLease(id, nodeID, epoch, engine string) *RemoteLease {
	return &RemoteLease{
		id:       id,
		nodeID:   nodeID,
		epoch:    epoch,
		engine:   engine,
		state:    leaseReserved,
		terminal: make(chan *millv1.AttemptResult, 1),
	}
}

func (l *RemoteLease) setState(s leaseState) {
	l.mu.Lock()
	l.state = s
	l.mu.Unlock()
}

func (l *RemoteLease) markCommitting() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.state == leaseDone {
		return false
	}
	if l.state == leaseReserved {
		l.state = leaseCommitting
	}
	return true
}

func (l *RemoteLease) markRunning() {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.state != leaseDone {
		l.state = leaseRunning
	}
}

func (l *RemoteLease) getState() leaseState {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.state
}

// only one caller gets to mark it done
func (l *RemoteLease) markDone() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.state == leaseDone {
		return false
	}
	l.state = leaseDone
	return true
}

func (l *RemoteLease) requestCancel(reason string) cancelAction {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.state == leaseDone {
		return cancelNoop
	}
	l.cancel = true
	l.reason = reason
	if l.state == leaseReserved {
		return cancelLocal
	}
	return cancelRemote
}

func (l *RemoteLease) cancelRequested() (bool, string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.cancel, l.reason
}

func (l *RemoteLease) releaseState() (leaseState, bool) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.released = true
	return l.state, l.cancel
}

func (l *RemoteLease) cleanupReady() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.released && l.state == leaseDone
}

func (l *RemoteLease) deliverCancelled(reason string) {
	l.deliverTerminal(&millv1.AttemptResult{
		Status: millv1.TerminalStatus_CANCELLED,
		Error:  reason,
	})
}

// hands the terminal to a waiting RunStep without blocking. duplicates
// (eg. reconnect replays) just drop, the channel holds one and the lease
// is already done
func (l *RemoteLease) deliverTerminal(res *millv1.AttemptResult) {
	if !l.markDone() {
		return
	}
	select {
	case l.terminal <- res:
	default:
	}
}

// mill's synthetic single step. real steps run on the executor and the
// mill never mirrors them
type remoteStep struct{}

func (remoteStep) Name() string          { return "remote execution" }
func (remoteStep) Command() string       { return "" }
func (remoteStep) Kind() models.StepKind { return models.StepKindSystem }

// what AcquireWorkflowSlot returns. Release unwinds placement
type millSlot struct {
	fleet *Mill
	lease *RemoteLease
	once  sync.Once
}

func (s *millSlot) Release() {
	if s == nil {
		return
	}
	s.once.Do(func() { s.fleet.releaseSlot(s) })
}
