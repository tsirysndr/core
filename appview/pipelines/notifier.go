package pipelines

import (
	"sync"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/notifier"
)

// StatusNotifier is a keyed broadcast notifier for pipeline status changes, keyed by the pipeline's AT URI.
//
// subscribers are notified whenever a status update arrives for that pipeline
type StatusNotifier struct {
	mu   sync.Mutex
	keys map[syntax.ATURI]*notifier.Notifier
}

func NewStatusNotifier() *StatusNotifier {
	return &StatusNotifier{
		keys: make(map[syntax.ATURI]*notifier.Notifier),
	}
}

func (n *StatusNotifier) Publish(uri syntax.ATURI) {
	n.mu.Lock()
	p, ok := n.keys[uri]
	n.mu.Unlock()
	if ok {
		p.NotifyAll()
	}
}

func (n *StatusNotifier) Subscribe(uri syntax.ATURI) chan struct{} {
	n.mu.Lock()
	p, ok := n.keys[uri]
	if !ok {
		nb := notifier.New()
		p = &nb
		n.keys[uri] = p
	}
	n.mu.Unlock()
	return p.Subscribe()
}

func (n *StatusNotifier) Unsubscribe(uri syntax.ATURI, ch chan struct{}) {
	n.mu.Lock()
	p, ok := n.keys[uri]
	n.mu.Unlock()
	if ok {
		p.Unsubscribe(ch)
	}
}
