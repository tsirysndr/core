package tapc

import "context"

type SimpleIndexer struct {
	EventHandler func(ctx context.Context, evt Event) error
	ErrorHandler func(ctx context.Context, err error)
}

var _ Handler = (*SimpleIndexer)(nil)

func (i *SimpleIndexer) OnEvent(ctx context.Context, evt Event) error {
	if i.EventHandler == nil {
		return nil
	}
	return i.EventHandler(ctx, evt)
}

func (i *SimpleIndexer) OnError(ctx context.Context, err error) {
	if i.ErrorHandler == nil {
		return
	}
	i.ErrorHandler(ctx, err)
}
