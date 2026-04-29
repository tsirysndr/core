//go:build !linux

package sandbox

import "fmt"

var ErrUnsupportedPlatform = fmt.Errorf("sandboxing is only supported on Linux")

func platformNew(_ LookupUID) (Backend, string) {
	return &NoopBackend{}, "sandboxing is not supported on this platform (Linux only)"
}

func platformProbe() string {
	return "sandboxing not supported (Linux only)"
}
