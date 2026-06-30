// extending code generated from sh.tangled.ci.subscribePipelineLogs

package tangled

import (
	"context"
	"fmt"
	"io"

	"github.com/bluesky-social/indigo/events"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	cbg "github.com/whyrusleeping/cbor-gen"
	extlexutil "tangled.org/core/lexutil"
)

// TODO: generate codes below from lexicon
type CiSubscribePipelineLogs_Event struct {
	Error   *events.ErrorFrame
	Control *CiSubscribePipelineLogs_Control
	Data    *CiSubscribePipelineLogs_Data

	// some private fields for internal routing perf
	Preserialized []byte `json:"-" cborgen:"-"`
}

func (xevt *CiSubscribePipelineLogs_Event) Serialize(wc io.Writer) error {
	header := events.EventHeader{Op: events.EvtKindMessage}
	var obj lexutil.CBOR

	switch {
	case xevt.Error != nil:
		header.Op = events.EvtKindErrorFrame
		obj = xevt.Error
	case xevt.Control != nil:
		header.MsgType = "#control"
		obj = xevt.Control
	case xevt.Data != nil:
		header.MsgType = "#data"
		obj = xevt.Data
	default:
		return fmt.Errorf("unrecognized event kind")
	}

	cborWriter := cbg.NewCborWriter(wc)
	if err := header.MarshalCBOR(cborWriter); err != nil {
		return fmt.Errorf("failed to write header: %w", err)
	}
	return obj.MarshalCBOR(cborWriter)
}

func (xevt *CiSubscribePipelineLogs_Event) Deserialize(r io.Reader) error {
	var header events.EventHeader
	if err := header.UnmarshalCBOR(r); err != nil {
		return fmt.Errorf("reading header: %w", err)
	}
	switch header.Op {
	case events.EvtKindMessage:
		switch header.MsgType {
		case "#control":
			var evt CiSubscribePipelineLogs_Control
			if err := evt.UnmarshalCBOR(r); err != nil {
				return fmt.Errorf("reading repoCommit event: %w", err)
			}
			xevt.Control = &evt
		case "#data":
			var evt CiSubscribePipelineLogs_Data
			if err := evt.UnmarshalCBOR(r); err != nil {
				return fmt.Errorf("reading repoSync event: %w", err)
			}
			xevt.Data = &evt
		}
	case events.EvtKindErrorFrame:
		var errframe events.ErrorFrame
		if err := errframe.UnmarshalCBOR(r); err != nil {
			return err
		}
		xevt.Error = &errframe
	default:
		return fmt.Errorf("unrecognized event stream type: %d", header.Op)
	}
	return nil
}

func CiSubscribePipelineLogs(ctx context.Context, c extlexutil.LexClient, pipeline string, workflows []string, sched extlexutil.Scheduler[CiSubscribePipelineLogs_Event]) error {
	defer sched.Shutdown()

	params := map[string]any{}
	params["pipeline"] = pipeline
	params["workflows"] = workflows

	return c.LexDo(ctx, extlexutil.Subscription, "", CiSubscribePipelineLogsNSID, params, nil, func(ctx context.Context, cr *cbg.CborReader) error {
		var evt CiSubscribePipelineLogs_Event
		if err := evt.Deserialize(cr); err != nil {
			return err
		}
		return sched.AddWork(ctx, "", &evt)
	})
}
