package config

import (
	"context"
	"testing"

	"tangled.org/core/consts"
)

func TestLoadConfig_DefaultKnotFallsBackToConst(t *testing.T) {
	t.Setenv("TANGLED_KNOT_DEFAULT", "")

	cfg, err := LoadConfig(context.Background())
	if err != nil {
		t.Fatalf("LoadConfig: %v", err)
	}
	if cfg.Knot.Default != consts.DefaultKnot {
		t.Fatalf("unset TANGLED_KNOT_DEFAULT = %q, want fallback %q", cfg.Knot.Default, consts.DefaultKnot)
	}
}

func TestLoadConfig_DefaultKnotHonorsOverride(t *testing.T) {
	t.Setenv("TANGLED_KNOT_DEFAULT", "kt.tngl.oyster.cafe")

	cfg, err := LoadConfig(context.Background())
	if err != nil {
		t.Fatalf("LoadConfig: %v", err)
	}
	if cfg.Knot.Default != "kt.tngl.oyster.cafe" {
		t.Fatalf("TANGLED_KNOT_DEFAULT override = %q, want kt.tngl.oyster.cafe", cfg.Knot.Default)
	}
}

func TestHostname_StripsPort(t *testing.T) {
	cases := map[string]string{
		"127.0.0.1:3000": "127.0.0.1",
		"localhost:3000": "localhost",
		"tangled.org":    "tangled.org",
		"[::1]:3000":     "::1",
	}
	for host, want := range cases {
		t.Run(host, func(t *testing.T) {
			c := &CoreConfig{AppviewHost: host}
			if got := c.Hostname(); got != want {
				t.Fatalf("Hostname() with AppviewHost=%q = %q, want %q", host, got, want)
			}
		})
	}
}
