package microvm

import (
	"bytes"
	"context"
	_ "embed"
	"fmt"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"text/template"
)

// https://www.iana.org/assignments/iana-ipv4-special-registry/iana-ipv4-special-registry.xhtml
// https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry.xhtml
// https://datatracker.ietf.org/doc/rfc6890/
var blockedNamespaceRoutes = []string{
	"0.0.0.0/8",       // unspecified / "this network" addresses
	"10.0.0.0/8",      // private network
	"100.64.0.0/10",   // shared carrier-grade nat space
	"127.0.0.0/8",     // loopback
	"169.254.0.0/16",  // link-local / autoconfiguration
	"172.16.0.0/12",   // private network
	"192.0.0.0/24",    // ietf protocol assignments
	"192.0.2.0/24",    // documentation / examples
	"192.88.99.0/24",  // deprecated 6to4 relay anycast
	"192.168.0.0/16",  // private network
	"198.18.0.0/15",   // benchmarking / testing
	"198.51.100.0/24", // documentation / examples
	"203.0.113.0/24",  // documentation / examples
	"224.0.0.0/4",     // multicast
	"240.0.0.0/4",     // reserved / future use, includes limited broadcast
	"::/128",          // unspecified address
	"::1/128",         // loopback
	"::ffff:0:0/96",   // ipv4-mapped addresses
	"64:ff9b::/96",    // ipv4/ipv6 translation prefix
	"100::/64",        // discard-only prefix
	"2001::/23",       // ietf protocol assignments
	"2001:db8::/32",   // documentation / examples
	"2002::/16",       // deprecated 6to4 addressing
	"fc00::/7",        // unique local addresses
	"fe80::/10",       // link-local unicast
	"ff00::/8",        // multicast
}

var blockedNamespaceNets = func() []*net.IPNet {
	nets := make([]*net.IPNet, 0, len(blockedNamespaceRoutes))
	for _, route := range blockedNamespaceRoutes {
		_, ipnet, err := net.ParseCIDR(route)
		if err != nil {
			panic(fmt.Sprintf("parse blocked route %q: %v", route, err))
		}
		nets = append(nets, ipnet)
	}
	return nets
}()

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

	args := []string{
		"--configure",
		"--mtu=" + netnsMTU,
	}
	if !n.dev {
		args = append(args, "--disable-host-loopback")
	}
	args = append(args,
		"--enable-sandbox",
		"--enable-seccomp",
		"--exit-fd=3",
		"--cidr="+outerSlirpCIDR,
		pid,
		netnsTapName,
	)

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
