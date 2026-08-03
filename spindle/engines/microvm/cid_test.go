package microvm

import (
	"testing"
)

func TestBuildConfigKeyScopesByRepo(t *testing.T) {
	spec := ImageSpec{BaseConfigHash: "deadbeef"}
	cfg := manifestConfig{Dependencies: []string{"nodejs"}}

	a, err := buildConfigKey(spec, cfg, "did:plc:aaa")
	if err != nil {
		t.Fatal(err)
	}
	b, err := buildConfigKey(spec, cfg, "did:plc:bbb")
	if err != nil {
		t.Fatal(err)
	}
	if a == b {
		t.Errorf("same config in different repos produced the same key %q", a)
	}

	again, err := buildConfigKey(spec, cfg, "did:plc:aaa")
	if err != nil {
		t.Fatal(err)
	}
	if a != again {
		t.Errorf("key not deterministic: %q vs %q", a, again)
	}
}
