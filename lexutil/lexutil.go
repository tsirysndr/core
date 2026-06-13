// extended version of indigo/lex/util package before upstreaming it
package lexutil

import (
	"context"
	"errors"
	"io"

	lexutil "github.com/bluesky-social/indigo/lex/util"
	cbg "github.com/whyrusleeping/cbor-gen"
)

type LexClient interface {
	lexutil.LexClient
	// LexSubscribe is basic event subscriber without redialing logic
	//
	// golang doesnt allow generics in method so we have to pass raw processFn here instead of Scheduler[T]
	LexSubscribe(ctx context.Context, endpoint string, params map[string]any, process func(ctx context.Context, cr *cbg.CborReader) error) error
}

const Subscription = "subscription"

var (
	ErrDialFailure = errors.New("dialing failed")
	ErrConnFailure = errors.New("connection failed")
)

type EventStreamMessage interface {
	Serialize(wc io.Writer) error
	Deserialize(r io.Reader) error
}

type Scheduler[T any] interface {
	AddWork(ctx context.Context, namespace string, val *T) error
	Shutdown()
}

type SeqScheduler[T any] interface {
	Scheduler[T]
	LastSeq() int64
}

type Redialer interface {
	// Process decodes the raw message and schedule it
	Process(ctx context.Context, cr *cbg.CborReader) error

	// UpdateParams increments the cursor parameter based on LastSeq stored in internal scheduler
	UpdateParams(ctx context.Context, params map[string]any) (updated bool)
}
