package spindle

import (
	"context"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	kgit "tangled.org/core/knotserver/git"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
)

func TestHasSkipCIPushOption(t *testing.T) {
	tests := []struct {
		name        string
		pushOptions []string
		want        bool
	}{
		{
			name:        "skip-ci requests skip",
			pushOptions: []string{"skip-ci"},
			want:        true,
		},
		{
			name:        "ci-skip requests skip",
			pushOptions: []string{"ci-skip"},
			want:        true,
		},
		{
			name:        "unrelated ci options do not skip",
			pushOptions: []string{"verbose-ci", "ci-verbose"},
			want:        false,
		},
		{
			name:        "empty options do not skip",
			pushOptions: []string{},
			want:        false,
		},
		{
			name:        "nil options do not skip",
			pushOptions: nil,
			want:        false,
		},
		{
			name:        "mixed options skip when any skip option appears",
			pushOptions: []string{"verbose-ci", "skip-ci", "ci-verbose"},
			want:        true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := kgit.HasSkipCIPushOption(tt.pushOptions)
			if got != tt.want {
				t.Fatalf("hasSkipCIPushOption(%v) = %v, want %v", tt.pushOptions, got, tt.want)
			}
		})
	}
}

func TestExecutorRoleBuildsMinimalSpindle(t *testing.T) {
	ctx := context.Background()
	dbPath := filepath.Join(t.TempDir(), "spindle.db")
	d, err := db.Make(ctx, dbPath)
	if err != nil {
		t.Fatalf("db.Make() error = %v", err)
	}

	cfg := &config.Config{Role: config.RoleExecutor}
	cfg.Server.DBPath = dbPath
	cfg.Server.Hostname = "executor.test"
	cfg.Server.Tap.Embed = true
	cfg.ArtifactStores.Disk.Dir = t.TempDir()
	cfg.Mill.ArtifactStore = "disk"

	s, err := New(ctx, cfg, d, map[string]models.Engine{})
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}

	if s.jc != nil || s.tap != nil || s.e != nil || s.ks != nil || s.res != nil || s.vault != nil {
		t.Fatal("executor role built coordinator-only spindle dependencies")
	}

	rr := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	s.Router().ServeHTTP(rr, req)
	if rr.Code != http.StatusOK {
		t.Fatalf("root status = %d, want %d", rr.Code, http.StatusOK)
	}

	rr = httptest.NewRecorder()
	req = httptest.NewRequest(http.MethodGet, "/xrpc/_health", nil)
	s.Router().ServeHTTP(rr, req)
	if rr.Code != http.StatusNotFound {
		t.Fatalf("executor xrpc status = %d, want %d", rr.Code, http.StatusNotFound)
	}
}
