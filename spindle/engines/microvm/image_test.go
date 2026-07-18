//go:build linux

package microvm

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeSpecFile(t *testing.T, path string) {
	t.Helper()
	data, err := json.Marshal(validImageSpec())
	if err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestResolveImageConventionalLayouts(t *testing.T) {
	cases := []struct {
		name   string
		layout func(t *testing.T, dir string)
	}{
		{
			name: "directory with spec.json",
			layout: func(t *testing.T, dir string) {
				writeSpecFile(t, filepath.Join(dir, "nixos", "spec.json"))
			},
		},
		{
			name: "flat <name>.json",
			layout: func(t *testing.T, dir string) {
				writeSpecFile(t, filepath.Join(dir, "nixos.json"))
			},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			tc.layout(t, dir)

			e := testEngine(t, dir)
			spec, path, name, err := e.resolveImage("nixos")
			if err != nil {
				t.Fatalf("resolveImage: %v", err)
			}
			if name != "nixos" {
				t.Fatalf("name = %q, want nixos", name)
			}
			if !strings.HasPrefix(path, dir) {
				t.Fatalf("resolved path %q not under image dir %q", path, dir)
			}
			if spec.Shell == "" {
				t.Fatal("resolved spec not loaded")
			}
		})
	}
}
func TestImageSpecPathPinsSymlinkTarget(t *testing.T) {
	dir := t.TempDir()
	imageA := filepath.Join(dir, "image-a")
	imageB := filepath.Join(dir, "image-b")
	writeSpecFile(t, filepath.Join(imageA, imageSpecFileName))
	writeSpecFile(t, filepath.Join(imageB, imageSpecFileName))

	alias := filepath.Join(dir, "default")
	if err := os.Symlink(imageA, alias); err != nil {
		t.Fatal(err)
	}
	got, ok, err := imageSpecPath(alias)
	if err != nil {
		t.Fatal(err)
	}
	if !ok {
		t.Fatal("imageSpecPath() did not resolve symlinked image")
	}

	if err := os.Remove(alias); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(imageB, alias); err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(imageA, imageSpecFileName)
	if got != want {
		t.Fatalf("imageSpecPath() = %q after alias resolution, want pinned target %q", got, want)
	}
}

func TestResolveImageDirectoryMissingSpec(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, "nixos"), 0o755); err != nil {
		t.Fatal(err)
	}

	e := testEngine(t, dir)
	_, _, _, err := e.resolveImage("nixos")
	if err == nil || !strings.Contains(err.Error(), imageSpecFileName) {
		t.Fatalf("directory without %s should error, got: %v", imageSpecFileName, err)
	}
}

func TestResolveImageRequiresImageDirOnlyWhenUsed(t *testing.T) {
	e := testEngine(t, "")
	_, _, _, err := e.resolveImage("nixos")
	if err == nil || !strings.Contains(err.Error(), "SPINDLE_MICROVM_PIPELINES_IMAGE_DIR") {
		t.Fatalf("missing image directory should error when resolving an image, got: %v", err)
	}
}

func TestResolveImageRejectsPaths(t *testing.T) {
	e := testEngine(t, t.TempDir())
	for _, name := range []string{"/etc/passwd", "../evil", "sub/evil", "..", "."} {
		if _, _, _, err := e.resolveImage(name); err == nil || !strings.Contains(err.Error(), "must be a plain name") {
			t.Fatalf("name %q should be rejected as a path, got: %v", name, err)
		}
	}
}

func validImageSpec() ImageSpec {
	return ImageSpec{
		Arch:     "x86_64",
		BootArgs: "console=ttyS0",
		Initrd:   "initrd",
		Kernel:   "kernel",
		RunnerConfig: RunnerConfig{
			Machine: "microvm",
		},
		MemoryMiB: 2048,
		Shell:     "/bin/sh",
		StoreDisk: "store-disk",
		VCPUs:     2,
	}
}

func TestImageSpecValidateWithoutBaseConfigHash(t *testing.T) {
	spec := validImageSpec()
	if err := spec.Validate(); err != nil {
		t.Fatalf("non-NixOS image spec should validate: %v", err)
	}
	if spec.SupportsConfigActivation() {
		t.Fatal("spec without baseConfigHash should not support config activation")
	}

	spec.BaseConfigHash = "abcdef"
	if !spec.SupportsConfigActivation() {
		t.Fatal("spec with baseConfigHash should support config activation")
	}
}

func TestImageSpecRequiresShell(t *testing.T) {
	spec := validImageSpec()
	spec.Shell = ""
	err := spec.Validate()
	if err == nil || !strings.Contains(err.Error(), "shell") {
		t.Fatalf("spec without shell should fail validation, got: %v", err)
	}
}
func TestMkfsExt4ForVolumesSkipsLookupWithoutVolumes(t *testing.T) {
	t.Setenv("PATH", "")
	path, err := mkfsExt4ForVolumes(nil, "")
	if err != nil {
		t.Fatalf("mkfsExt4ForVolumes() with no volumes returned error: %v", err)
	}
	if path != "" {
		t.Fatalf("mkfsExt4ForVolumes() = %q with no volumes, want empty path", path)
	}

	_, err = mkfsExt4ForVolumes([]Volume{{Image: "workspace"}}, "")
	if err == nil || !strings.Contains(err.Error(), "mkfs.ext4") {
		t.Fatalf("mkfsExt4ForVolumes() with a volume and no formatter returned %v", err)
	}
}

func TestQEMURunnerValidateRejectsUnsupportedNetworkTypeBeforeHostChecks(t *testing.T) {
	spec := validImageSpec()
	spec.NetworkInterfaces = []NetworkInterface{{
		Type: "tap",
		ID:   "net0",
		MAC:  "02:00:00:00:00:01",
	}}
	err := (qemuRunner{}).Validate(spec, false)
	if err == nil || !strings.Contains(err.Error(), `unsupported microvm network interface type "tap"`) {
		t.Fatalf("Validate() error = %v, want unsupported network type", err)
	}
}
