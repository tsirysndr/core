package microvm

import (
	"context"
	"errors"
	"log/slog"
	"testing"

	"tangled.org/core/spindle/models"
)

type shutdownTestVM struct {
	exited      bool
	waitErr     error
	shutdownErr error
	closed      bool
}

func (v *shutdownTestVM) Shutdown(context.Context) error {
	v.exited = true
	return v.shutdownErr
}

func (v *shutdownTestVM) WaitContext(ctx context.Context) error {
	if v.exited {
		return v.waitErr
	}
	return ctx.Err()
}

func (v *shutdownTestVM) Close() error {
	v.closed = true
	return nil
}

func (*shutdownTestVM) Logs() VMLogs    { return VMLogs{} }
func (*shutdownTestVM) CID() uint32     { return 0 }
func (*shutdownTestVM) WorkDir() string { return "" }
func (*shutdownTestVM) OOMKilled() bool { return false }

func TestShutdownVM_TreatsExitedVMAsCleanedUp(t *testing.T) {
	for name, vm := range map[string]*shutdownTestVM{
		"already exited": {
			exited:      true,
			waitErr:     errors.New("qemu exited"),
			shutdownErr: errors.New("qmp broken pipe"),
		},
		"exits during fallback": {
			waitErr:     errors.New("qemu exited"),
			shutdownErr: errors.New("qmp broken pipe"),
		},
	} {
		t.Run(name, func(t *testing.T) {
			e := &Engine{l: slog.Default()}
			state := &workflowState{VM: vm}

			if err := e.shutdownVM(context.Background(), models.WorkflowId{}, state); err != nil {
				t.Fatalf("shutdownVM: %v", err)
			}
			if !vm.closed {
				t.Fatal("expected vm handle to be closed")
			}
		})
	}
}
