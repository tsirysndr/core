package config

import (
	"context"
	"testing"
)

func TestLoadAllowsUnconfiguredMicroVMEngine(t *testing.T) {
	t.Setenv("SPINDLE_SERVER_HOSTNAME", "spindle.example.com")
	t.Setenv("SPINDLE_SERVER_OWNER", "did:web:spindle.example.com")
	t.Setenv("SPINDLE_MICROVM_PIPELINES_IMAGE_DIR", "")

	cfg, err := Load(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if cfg.MicroVMPipelines.ImageDir != "" {
		t.Fatalf("image directory = %q, want empty", cfg.MicroVMPipelines.ImageDir)
	}
}

func TestLoadRequiresJumpHostKey(t *testing.T) {
	t.Setenv("SPINDLE_SERVER_HOSTNAME", "spindle.example.com")
	t.Setenv("SPINDLE_SERVER_OWNER", "did:web:spindle.example.com")
	t.Setenv("SPINDLE_ROLE", "mill")
	t.Setenv("SPINDLE_MILL_JUMP_LISTEN_ADDR", "0.0.0.0:22")

	if _, err := Load(context.Background()); err == nil {
		t.Fatal("Load accepted a jump listener without a host key path")
	}
}

func TestLoadRejectsJumpListenerOutsideMill(t *testing.T) {
	t.Setenv("SPINDLE_SERVER_HOSTNAME", "spindle.example.com")
	t.Setenv("SPINDLE_SERVER_OWNER", "did:web:spindle.example.com")
	t.Setenv("SPINDLE_ROLE", "standalone")
	t.Setenv("SPINDLE_MILL_JUMP_LISTEN_ADDR", "0.0.0.0:22")
	t.Setenv("SPINDLE_MILL_JUMP_HOST_KEY_PATH", "/tmp/jump-host-key")

	if _, err := Load(context.Background()); err == nil {
		t.Fatal("Load accepted a mill jump listener in standalone mode")
	}
}

func TestLoadValidatesMaxJumpConnections(t *testing.T) {
	t.Setenv("SPINDLE_SERVER_HOSTNAME", "spindle.example.com")
	t.Setenv("SPINDLE_SERVER_OWNER", "did:web:spindle.example.com")
	t.Setenv("SPINDLE_ROLE", "mill")
	t.Setenv("SPINDLE_MILL_JUMP_LISTEN_ADDR", "0.0.0.0:22")
	t.Setenv("SPINDLE_MILL_JUMP_HOST_KEY_PATH", "/tmp/jump-host-key")
	t.Setenv("SPINDLE_MILL_MAX_JUMP_CONNECTIONS", "0")

	if _, err := Load(context.Background()); err == nil {
		t.Fatal("Load accepted a non-positive jump connection limit")
	}

	t.Setenv("SPINDLE_MILL_MAX_JUMP_CONNECTIONS", "17")
	cfg, err := Load(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Mill.MaxJumpConnections != 17 {
		t.Fatalf("max jump connections = %d, want 17", cfg.Mill.MaxJumpConnections)
	}
}
