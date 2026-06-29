package engine

import (
	"strings"
	"testing"
)

type testManifest struct {
	Image        string              `yaml:"image"`
	Registry     map[string]any      `yaml:"registry"`
	Dependencies []string            `yaml:"dependencies"`
	Nested       map[string][]string `yaml:"nested"`
	Steps        []struct {
		Name string `yaml:"name"`
	} `yaml:"steps"`
}

func TestDescribeManifestError(t *testing.T) {
	cases := []struct {
		name string
		raw  string
		want []string // substrings the message must contain
	}{
		{
			name: "map field written as list",
			raw:  "registry:\n  - nixpkgs: github:nixos/nixpkgs\n",
			want: []string{"registry", "a mapping", "a list"},
		},
		{
			name: "list field written as scalar",
			raw:  "dependencies: bun\n",
			want: []string{"dependencies", "a list", "a scalar value"},
		},
		{
			name: "scalar field written as mapping",
			raw:  "image:\n  name: nixos\n",
			want: []string{"image", "a scalar value", "a mapping"},
		},
		{
			name: "nested map value mis-shaped",
			raw:  "nested:\n  foo: bar\n", // foo should be a list of strings
			want: []string{"nested.foo", "a list", "a scalar value"},
		},
		{
			name: "field inside a list element mis-shaped",
			raw:  "steps:\n  - name:\n      x: y\n", // steps[0].name should be a scalar
			want: []string{"steps[0].name", "a scalar value", "a mapping"},
		},
		{
			name: "unknown top-level field (typo)",
			raw:  "dependancies:\n  - bun\n",
			want: []string{"unknown field", "dependancies"},
		},
		{
			name: "unknown field inside a list element",
			raw:  "steps:\n  - name: x\n    cmd: y\n", // it's `command`, not `cmd`
			want: []string{"unknown field", "steps[0].cmd"},
		},
		{
			// `name` (filename-sourced, yaml:"-") must be tolerated so the real
			// typo `registre` on a later line is the thing that surfaces.
			name: "tolerated name does not mask a later typo",
			raw:  "name: mill\nengine: microvm\nregistre:\n  - x: y\n",
			want: []string{"unknown field", "registre"},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			err := DescribeManifestError(tc.raw, testManifest{})
			if err == nil {
				t.Fatalf("expected an error, got nil")
			}
			for _, w := range tc.want {
				if !strings.Contains(err.Error(), w) {
					t.Errorf("error %q missing %q", err, w)
				}
			}
		})
	}
}

func TestDescribeManifestErrorNoFalsePositives(t *testing.T) {
	cases := []string{
		// well-formed manifest
		"image: nixos\nregistry:\n  nixpkgs: github:nixos/nixpkgs\ndependencies:\n  - bun\n",
		// empty value is harmless, not a mismatch
		"image: nixos\nregistry:\n",
		// generic workflow keys live in the same doc and aren't engine fields,
		// but must not be flagged as unknown at the root
		"engine: microvm\nwhen:\n  - event: [push]\nclone:\n  skip: true\nimage: nixos\n",
		// `any` map values accept any shape, including nested lists/maps
		"registry:\n  k:\n    - a\n    - b\n",
		// well-formed nested map-of-lists
		"nested:\n  foo:\n    - a\n    - b\n",
		// user-defined map keys are data, never flagged as unknown fields
		"nested:\n  any-package-name:\n    - a\n",
	}
	for _, raw := range cases {
		if err := DescribeManifestError(raw, testManifest{}); err != nil {
			t.Errorf("DescribeManifestError(%q) = %v, want nil", raw, err)
		}
	}
}

func TestDescribeManifestErrorPointerSchema(t *testing.T) {
	// nixery passes a pointer to an anonymous struct
	schema := &testManifest{}
	err := DescribeManifestError("nested: oops\n", schema)
	if err == nil || !strings.Contains(err.Error(), "nested") {
		t.Fatalf("expected an error naming `nested`, got %v", err)
	}
}
