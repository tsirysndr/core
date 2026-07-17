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
