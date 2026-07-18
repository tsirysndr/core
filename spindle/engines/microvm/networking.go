//go:build linux

package microvm

import (
	"bytes"
	"context"
	_ "embed"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"text/template"

	"tangled.org/core/spindle/netguard"
)

var (
	blockedNamespaceRoutes = netguard.BlockedRoutes
	blockedNamespaceNets   = netguard.BlockedNets
)

//go:embed netns_wrapper.sh.tmpl
var netnsWrapperTemplate string

type netnsWrapperData struct {
	TapName       string
	BlockedRoutes []string
}

func writeNetnsWrapper(path string, dev bool) error {
	tmpl, err := template.New("netns-wrapper").Parse(netnsWrapperTemplate)
	if err != nil {
		return fmt.Errorf("parse qemu network namespace wrapper template: %w", err)
	}

	var script bytes.Buffer

	var routes []string
	if !dev {
		routes = blockedNamespaceRoutes
	}

	err = tmpl.Execute(&script, netnsWrapperData{
		TapName:       netnsTapName,
		BlockedRoutes: routes,
	})
	if err != nil {
		return fmt.Errorf("render qemu network namespace wrapper template: %w", err)
	}

	if err := os.WriteFile(path, script.Bytes(), 0o700); err != nil {
		return fmt.Errorf("write qemu network namespace wrapper: %w", err)
	}

	return nil
}

type slirpNamespace struct {
	spec    ImageSpec
	pidFile string
	dev     bool
}

func (n *slirpNamespace) Start(ctx context.Context, logFile *os.File, logger *slog.Logger) (*exec.Cmd, *os.File, error) {
	pid, err := waitForPIDFile(ctx, n.pidFile)
	if err != nil {
		return nil, nil, err
	}

	exitR, exitW, err := os.Pipe()
	if err != nil {
		return nil, nil, fmt.Errorf("create slirp4netns exit pipe: %w", err)
	}
	defer exitR.Close() // always close our read end; child gets it via ExtraFiles dup

	var ok bool
	defer func() {
		if !ok {
			_ = exitW.Close()
		}
	}()

	slirpPath, err := exec.LookPath("slirp4netns")
	if err != nil {
		return nil, nil, fmt.Errorf("slirp4netns command not found in PATH: %w", err)
	}

	args := slirpArgs(n.dev, pid)

	cmd := exec.CommandContext(ctx, slirpPath, args...)
	cmd.ExtraFiles = []*os.File{exitR}
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	if err := cmd.Start(); err != nil {
		return nil, nil, fmt.Errorf("start slirp4netns: %w", err)
	}
	logger.Info("started slirp4netns network namespace", "pid", pid, "cidr", outerSlirpCIDR, "tap", netnsTapName)

	ok = true
	return cmd, exitW, nil
}

func slirpArgs(dev bool, pid string) []string {
	args := []string{
		"--configure",
		"--mtu=" + netnsMTU,
	}
	if !dev {
		args = append(args, "--disable-host-loopback")
	}
	args = append(args,
		"--disable-dns",
		"--enable-seccomp",
		"--exit-fd=3",
		"--cidr="+outerSlirpCIDR,
		pid,
		netnsTapName,
	)
	return args
}
