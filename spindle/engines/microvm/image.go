package microvm

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const imageSpecFileName = "spec.json"

type RunnerConfig struct {
	CPU       string   `json:"cpu,omitempty"`
	Machine   string   `json:"machine,omitempty"`
	Console   string   `json:"console,omitempty"`
	ExtraArgs []string `json:"extraArgs,omitempty"`
}

type ImageSpec struct {
	Arch              string             `json:"arch"`
	BootArgs          string             `json:"bootArgs"`
	Initrd            string             `json:"initrd"`
	Kernel            string             `json:"kernel"`
	RunnerType        string             `json:"runnerType"`
	RunnerConfig      RunnerConfig       `json:"runnerConfig"`
	MemoryMiB         int                `json:"memoryMiB"`
	NetworkInterfaces []NetworkInterface `json:"networkInterfaces"`
	StoreDisk         string             `json:"storeDisk"`
	StoreDiskType     string             `json:"storeDiskType"`
	// baseConfigHash identifies the base nixos configuration baked into the
	// image. its only for nixos images as other images won't have a system
	// to rebuild.
	BaseConfigHash string `json:"baseConfigHash,omitempty"`
	// shell is the login shell used to run workflow step commands in the guest.
	Shell   string   `json:"shell"`
	VCPUs   int      `json:"vcpus"`
	Volumes []Volume `json:"volumes"`
}

func (s ImageSpec) SupportsConfigActivation() bool {
	return s.BaseConfigHash != ""
}

type NetworkInterface struct {
	Type string `json:"type"`
	ID   string `json:"id"`
	MAC  string `json:"mac"`
}

type Volume struct {
	FSType     string `json:"fsType"`
	Image      string `json:"image"`
	ImageType  string `json:"imageType"`
	MountPoint string `json:"mountPoint"`
	ReadOnly   bool   `json:"readOnly"`
	SizeMiB    int64  `json:"sizeMiB"`
}

func LoadImageSpec(path string) (ImageSpec, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return ImageSpec{}, fmt.Errorf("read microvm image spec: %w", err)
	}

	var spec ImageSpec
	if err := json.Unmarshal(data, &spec); err != nil {
		return ImageSpec{}, fmt.Errorf("parse microvm image spec: %w", err)
	}

	base := filepath.Dir(path)
	spec.Kernel = resolveImageSpecPath(base, spec.Kernel)
	spec.Initrd = resolveImageSpecPath(base, spec.Initrd)
	spec.StoreDisk = resolveImageSpecPath(base, spec.StoreDisk)

	if err := spec.Validate(); err != nil {
		return ImageSpec{}, err
	}
	return spec, nil
}

func (s ImageSpec) Validate() error {
	if s.Kernel == "" {
		return fmt.Errorf("microvm image spec missing kernel")
	}
	if s.Initrd == "" {
		return fmt.Errorf("microvm image spec missing initrd")
	}
	if s.StoreDisk == "" {
		return fmt.Errorf("microvm image spec missing storeDisk")
	}
	if s.BootArgs == "" {
		return fmt.Errorf("microvm image spec missing bootArgs")
	}
	if s.Shell == "" {
		return fmt.Errorf("microvm image spec missing shell")
	}
	if s.RunnerType == "qemu" || s.RunnerType == "" {
		if s.RunnerConfig.Machine == "" {
			return fmt.Errorf("microvm image spec missing runnerConfig.machine for qemu runner")
		}
	}
	if s.MemoryMiB <= 0 {
		return fmt.Errorf("microvm image spec memoryMiB must be positive")
	}
	if s.VCPUs <= 0 {
		return fmt.Errorf("microvm image spec vcpus must be positive")
	}
	for _, networkInterface := range s.NetworkInterfaces {
		if networkInterface.Type == "" {
			return fmt.Errorf("microvm image spec network interface missing type")
		}
		if networkInterface.ID == "" {
			return fmt.Errorf("microvm image spec network interface missing id")
		}
		if networkInterface.MAC == "" {
			return fmt.Errorf("microvm image spec network interface %q missing mac", networkInterface.ID)
		}
	}
	for _, volume := range s.Volumes {
		if volume.Image == "" {
			return fmt.Errorf("microvm image spec volume missing image")
		}
		if volume.FSType == "" {
			return fmt.Errorf("microvm image spec volume %q missing fsType", volume.Image)
		}
		if volume.SizeMiB <= 0 {
			return fmt.Errorf("microvm image spec volume %q sizeMiB must be positive", volume.Image)
		}
	}
	return nil
}

func (s ImageSpec) RunnerCmd() string {
	switch s.RunnerType {
	case "qemu", "":
		return "qemu-system-" + s.Arch
	case "firecracker":
		return "firecracker"
	default:
		return ""
	}
}

// also see Runner.Validate for where Runner specific files are validated
func (s ImageSpec) validateImageFiles() error {
	required := map[string]string{
		"kernel":    s.Kernel,
		"initrd":    s.Initrd,
		"storeDisk": s.StoreDisk,
	}
	for name, path := range required {
		if !filepath.IsAbs(path) {
			continue
		}
		if _, err := os.Stat(path); err != nil {
			return fmt.Errorf("required image spec file %s not found at %q: %w", name, path, err)
		}
	}

	return nil
}

func resolveImageSpecPath(base, path string) string {
	if path == "" || filepath.IsAbs(path) {
		return path
	}
	return filepath.Join(base, path)
}

func (e *Engine) resolveImage(name string) (ImageSpec, string, string, error) {
	name = strings.TrimSpace(name)
	if name == "" {
		name = strings.TrimSpace(e.cfg.MicroVMPipelines.DefaultImage)
	}
	if name == "" {
		return ImageSpec{}, "", "", fmt.Errorf("no image specified in workflow and SPINDLE_MICROVM_PIPELINES_DEFAULT_IMAGE is not set")
	}
	if !isPlainImageName(name) {
		return ImageSpec{}, "", "", fmt.Errorf("invalid microVM image name %q: must be a plain name, not a path", name)
	}

	imageDir := strings.TrimSpace(e.cfg.MicroVMPipelines.ImageDir)
	if imageDir == "" {
		return ImageSpec{}, "", "", fmt.Errorf("microVM workflows require SPINDLE_MICROVM_PIPELINES_IMAGE_DIR")
	}

	candidates := imageCandidates(imageDir, name)
	for _, candidate := range candidates {
		path, ok, err := imageSpecPath(candidate)
		if err != nil {
			return ImageSpec{}, "", "", err
		}
		if !ok {
			continue
		}
		imageSpec, err := LoadImageSpec(path)
		if err != nil {
			return ImageSpec{}, "", "", err
		}
		return imageSpec, path, name, nil
	}

	return ImageSpec{}, "", "", fmt.Errorf("microVM image %q was not found; looked in: %s", name, strings.Join(candidates, ", "))
}

// check if image name is not a path
func isPlainImageName(name string) bool {
	if name == "" || name == "." || name == ".." {
		return false
	}
	if filepath.IsAbs(name) || strings.ContainsRune(name, '/') || strings.ContainsRune(name, filepath.Separator) {
		return false
	}
	return true
}

// returns candidates, which is either a directory or spec file itself
func imageCandidates(imageDir, name string) []string {
	if imageDir == "" {
		return nil
	}
	return []string{
		filepath.Join(imageDir, name),
		filepath.Join(imageDir, name+".json"),
	}
}

// resolve the candidate to a spec:
// - first check if its a file, if yes, return
// - otherwise assume its a directory and check and return `/spec.json`
func imageSpecPath(candidate string) (string, bool, error) {
	info, err := os.Stat(candidate)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return "", false, nil
		}
		return "", false, err
	}
	if !info.IsDir() {
		return candidate, true, nil
	}

	spec := filepath.Join(candidate, imageSpecFileName)
	info, err = os.Stat(spec)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return "", false, fmt.Errorf("microVM image directory %q does not contain %s", candidate, imageSpecFileName)
		}
		return "", false, err
	}
	// this only happens if there is a directory named `spec.json` which would be very silly.
	// but better output an error for it anyway :p
	if info.IsDir() {
		return "", false, fmt.Errorf("microVM image spec %q is a directory", spec)
	}
	return spec, true, nil
}
