package nixery

import (
	"context"
	"testing"

	"tangled.org/core/spindle/config"
)

func TestNewDefersDockerClientUntilWorkflowSetup(t *testing.T) {
	t.Setenv("DOCKER_HOST", "tcp://127.0.0.1:2376")
	t.Setenv("DOCKER_TLS_VERIFY", "1")
	t.Setenv("DOCKER_CERT_PATH", t.TempDir())

	e, err := New(context.Background(), &config.Config{})
	if err != nil {
		t.Fatal(err)
	}
	if e.docker != nil {
		t.Fatal("docker client initialized during engine initialization")
	}
	if _, err := e.ensureDocker(); err == nil {
		t.Fatal("expected incomplete Docker TLS configuration to fail when first used")
	}
}
