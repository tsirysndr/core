//go:build linux

package main

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"time"

	"github.com/mdlayher/vsock"
	"github.com/urfave/cli/v3"
	agentv1 "tangled.org/core/spindle/agentproto/gen"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engines/microvm"
)

func SpindleMicroVMRunCommand() *cli.Command {
	return &cli.Command{
		Name:  "spindle-microvm-run",
		Usage: "launch the Spindle base microVM and run one command over vsock",
		Flags: []cli.Flag{
			&cli.StringFlag{
				Name:    "image-spec",
				Sources: cli.EnvVars("SPINDLE_MICROVM_IMAGE_SPEC"),
				Usage:   "path to microVM image spec JSON",
			},
			&cli.StringFlag{
				Name:  "mkfs-ext4",
				Usage: "override mkfs.ext4 binary",
			},
			&cli.StringFlag{
				Name:  "work-dir",
				Usage: "directory for per-run disks and sockets",
			},
			&cli.UintFlag{
				Name:  "cid",
				Usage: "guest vsock CID; defaults to a random high CID",
			},
			&cli.UintFlag{
				Name:  "port",
				Value: 10240,
				Usage: "host vsock port to listen on",
			},
			&cli.UintFlag{
				Name:  "memory-mib",
				Usage: "override the guest memory size in MiB (defaults to the image spec)",
			},
			&cli.BoolFlag{
				Name:  "disable-kvm",
				Usage: "run without -enable-kvm even if /dev/kvm is available",
			},
			&cli.BoolFlag{
				Name:  "dev",
				Usage: "enable dev mode (allows host network access, disables SSL verification)",
			},
			&cli.DurationFlag{
				Name:  "qmp-timeout",
				Value: 10 * time.Second,
				Usage: "how long to wait for qmp to become ready",
			},
			&cli.DurationFlag{
				Name:  "accept-timeout",
				Value: 15 * time.Second,
				Usage: "how long to wait for the guest agent after qemu starts",
			},
			&cli.DurationFlag{
				Name:  "exec-timeout",
				Value: 30 * time.Second,
				Usage: "timeout for the guest command",
			},
			&cli.DurationFlag{
				Name:    "cache-upload-wait-timeout",
				Aliases: []string{"cache-drain-timeout"},
				Value:   5 * time.Minute,
				Usage:   "how long to wait for guest cache uploads to finish after the command exits",
			},
			&cli.DurationFlag{
				Name:  "shutdown-timeout",
				Value: 10 * time.Second,
				Usage: "how long to wait for qemu to exit after guest powerdown",
			},
			&cli.StringFlag{
				Name:  "cwd",
				Usage: "guest working directory",
			},
			&cli.StringSliceFlag{
				Name:    "cache-read-url",
				Sources: cli.EnvVars("SPINDLE_NIX_CACHE_READ_URLS"),
				Usage:   "Nix binary cache URL to pass to the guest; repeatable",
			},
			&cli.StringSliceFlag{
				Name:    "cache-trusted-public-key",
				Sources: cli.EnvVars("SPINDLE_NIX_CACHE_TRUSTED_PUBLIC_KEYS"),
				Usage:   "Nix binary cache public key to trust in the guest; repeatable",
			},
			&cli.StringFlag{
				Name:    "cache-upload-url",
				Sources: cli.EnvVars("SPINDLE_NIX_CACHE_UPLOAD_URL"),
				Usage:   "optional cache upload URL for guest-built store paths",
			},
			&cli.StringFlag{
				Name:  "activate-config",
				Usage: "JSON user config to activate before exec (e.g. '{\"services\":{\"openssh\":{\"enable\":true}}}')",
			},
			&cli.StringFlag{
				Name:  "db",
				Usage: "path to sqlite database for config cache",
			},
		},
		Action: runMicroVMRunDev,
	}
}

func runMicroVMRunDev(ctx context.Context, cmd *cli.Command) error {
	imageSpecPath := cmd.String("image-spec")
	if imageSpecPath == "" {
		return fmt.Errorf("--image-spec or SPINDLE_MICROVM_IMAGE_SPEC is required")
	}

	imageSpec, err := microvm.LoadImageSpec(imageSpecPath)
	if err != nil {
		return err
	}

	port := uint32(cmd.Uint("port"))
	// tell the guest which host vsock port to dial back on. shuttle reads the
	// cmdline params this is so we can run multiple of this process
	// concurrently, because otherwise it listens on a specific vsock port, and
	// we cant bind to the same port twice...
	imageSpec.BootArgs = fmt.Sprintf("%s shuttle.vsock_port=%d", imageSpec.BootArgs, port)
	if mib := cmd.Uint("memory-mib"); mib > 0 {
		imageSpec.MemoryMiB = int(mib)
	}
	ln, err := vsock.Listen(port, nil)
	if err != nil {
		return fmt.Errorf("listen on vsock port %d: %w", port, err)
	}
	defer ln.Close()

	vm, err := microvm.StartVM(ctx, microvm.VMConfig{
		Image:       imageSpec,
		BootTimeout: cmd.Duration("qmp-timeout"),
		CID:         uint32(cmd.Uint("cid")),
		EnableKVM:   !cmd.Bool("disable-kvm"),
		MkfsExt4:    cmd.String("mkfs-ext4"),
		WorkDir:     cmd.String("work-dir"),
		Dev:         cmd.Bool("dev"),
	}, slog.Default())
	if err != nil {
		return err
	}
	defer vm.Close()

	logs := vm.Logs()
	fmt.Fprintf(os.Stderr, "microvm started: cid=%d work-dir=%s serial-log=%s qemu-log=%s\n",
		vm.CID(),
		vm.WorkDir(),
		logs.Serial,
		logs.Extra["qemu"],
	)

	logger := slog.Default()

	if cmd.Duration("accept-timeout") > 0 {
		if err := ln.SetDeadline(time.Now().Add(cmd.Duration("accept-timeout"))); err != nil {
			return fmt.Errorf("set accept deadline: %w", err)
		}
	}

	argv := cmd.Args().Slice()
	if len(argv) == 0 {
		argv = []string{"/run/current-system/sw/bin/echo", "hello-from-spindle"}
	}
	jobID := "spindle-microvm-run"
	execID := "dev-1"
	var pendingConfigKey string
	var pendingConfigToplevel string
	var configCacheDB *db.DB

	fmt.Fprintf(os.Stderr, "listening for agent on %s\n", ln.Addr())
	conn, err := acceptExpectedVsockConn(ln, vm.CID(), logger)
	if err != nil {
		return fmt.Errorf("accept agent connection: %w", err)
	}
	defer conn.Close()

	upstreams, err := microvm.BuildCacheUpstreams(cmd.StringSlice("cache-read-url"), nil)
	if err != nil {
		return fmt.Errorf("build cache upstreams: %w", err)
	}

	var readCache *microvm.ReadCacheProxy
	if len(cmd.StringSlice("cache-read-url")) > 0 {
		var err error
		readCache, err = microvm.StartReadCacheProxy(ctx, vm.CID(), upstreams, logger)
		if err != nil {
			return fmt.Errorf("start read cache proxy: %w", err)
		}
		defer readCache.Close()
	}

	var uploadCache *microvm.UploadCacheProxy
	if cmd.String("cache-upload-url") != "" {
		var err error
		uploadCache, err = microvm.StartUploadCacheProxy(ctx, vm.CID(), cmd.String("cache-upload-url"), upstreams, filepath.Join(vm.WorkDir(), "upload-cache"), logger)
		if err != nil {
			return fmt.Errorf("start upload cache proxy: %w", err)
		}
		defer uploadCache.Close()
	}
	dnsProxy, err := microvm.StartDNSProxy(ctx, vm.CID(), logger)
	if err != nil {
		return fmt.Errorf("start dns proxy: %w", err)
	}
	defer dnsProxy.Close()

	session := microvm.NewAgentSession(conn, logger)

	initCtx, cancelInit := context.WithTimeout(ctx, 30*time.Second)
	defer cancelInit()
	if err := session.Init(initCtx, &agentv1.Init{
		JobId:                  jobID,
		CacheTrustedPublicKeys: cmd.StringSlice("cache-trusted-public-key"),
		CacheReadProxyPort:     readCache.Port(),
		CacheUploadProxyPort:   uploadCache.Port(),
		DnsProxyPort:           dnsProxy.Port(),
	}); err != nil {
		return fmt.Errorf("init agent: %w", err)
	}

	execCtx := ctx
	if cmd.Duration("exec-timeout") > 0 {
		var cancel context.CancelFunc
		execCtx, cancel = context.WithTimeout(ctx, cmd.Duration("exec-timeout"))
		defer cancel()
	}

	if cmd.String("activate-config") != "" {
		actCtx := execCtx
		baseHash, err := microvm.BaseConfigHash(imageSpec)
		if err != nil {
			return fmt.Errorf("calculate base config hash: %w", err)
		}

		var configKey string
		var cachedToplevel string
		if cmd.String("db") != "" {
			configCacheDB, err = db.Make(ctx, cmd.String("db"))
			if err != nil {
				return fmt.Errorf("failed to open database: %w", err)
			}
			defer configCacheDB.Close()

			configKey, err = microvm.BuildConfigKey(imageSpec, cmd.String("activate-config"))
			if err != nil {
				return fmt.Errorf("calculate config key: %w", err)
			}

			record, err := configCacheDB.GetNixOSToplevelCacheRecord(configKey)
			if err != nil {
				if !errors.Is(err, sql.ErrNoRows) {
					return fmt.Errorf("lookup config cache: %w", err)
				}
			} else {
				cachedToplevel = record.Toplevel
				fmt.Printf("realizing cached NixOS config %s\n", cachedToplevel)
			}
		}

		result, err := session.ActivateConfig(actCtx, "dev-activate", &agentv1.ActivateConfig{
			ConfigKey:      configKey,
			BaseConfigHash: baseHash,
			UserConfig:     cmd.String("activate-config"),
			Toplevel:       cachedToplevel,
		})
		if err != nil {
			return fmt.Errorf("activate config: %w", err)
		}
		fmt.Fprintf(os.Stderr, "activated config toplevel: %s\n", result.Toplevel)

		if configCacheDB != nil && cachedToplevel == "" && result.Toplevel != "" && configKey != "" {
			if uploadCache == nil {
				fmt.Fprintln(os.Stderr, "skipping config cache metadata commit: no cache upload url configured")
			} else {
				pendingConfigKey = configKey
				pendingConfigToplevel = result.Toplevel
			}
		}
	}

	exitCode, err := session.Exec(execCtx, microvm.AgentExec{
		ID: execID,
		ExecStart: &agentv1.ExecStart{
			Argv: argv,
			Cwd:  cmd.String("cwd"),
		},
		Stdout: os.Stdout,
		Stderr: os.Stderr,
	})
	if err != nil {
		return err
	}

	if uploadCache != nil {
		uploadWaitCtx := ctx
		if cmd.Duration("cache-upload-wait-timeout") > 0 {
			var cancel context.CancelFunc
			uploadWaitCtx, cancel = context.WithTimeout(ctx, cmd.Duration("cache-upload-wait-timeout"))
			defer cancel()
		}
		uploaded, err := session.Drain(uploadWaitCtx)
		if err != nil {
			return err
		}
		fmt.Printf("cache uploaded: %d\n", uploaded)
		if configCacheDB != nil && pendingConfigKey != "" && pendingConfigToplevel != "" {
			if err := configCacheDB.SaveNixOSToplevelCacheRecord(pendingConfigKey, pendingConfigToplevel); err != nil {
				return fmt.Errorf("save config cache: %w", err)
			}
		}
	}

	// mirror the engine shutdown order: ask the agent to power off first,
	// then fall back to qemu powerdown / kill
	shutdownCtx, cancel := context.WithTimeout(context.Background(), cmd.Duration("shutdown-timeout"))
	defer cancel()
	poweredOff := false
	if err := session.Poweroff(shutdownCtx); err != nil {
		fmt.Fprintf(os.Stderr, "agent poweroff: %s\n", err)
	} else if err := vm.WaitContext(shutdownCtx); err == nil {
		poweredOff = true
	}
	if !poweredOff {
		if err := vm.Shutdown(shutdownCtx); err != nil {
			fmt.Fprintf(os.Stderr, "microvm shutdown fallback: %s\n", err)
		}
	}

	if exitCode != 0 {
		return fmt.Errorf("guest command exited with code %d", exitCode)
	}
	return nil
}

func acceptExpectedVsockConn(ln *vsock.Listener, allowedCID uint32, logger *slog.Logger) (net.Conn, error) {
	for {
		conn, err := ln.Accept()
		if err != nil {
			return nil, err
		}
		if allowedCID == 0 {
			return conn, nil
		}
		addr, ok := conn.RemoteAddr().(*vsock.Addr)
		if ok && addr.ContextID == allowedCID {
			return conn, nil
		}
		remote := conn.RemoteAddr()
		_ = conn.Close()
		logger.Warn("dropped agent connection from unexpected cid", "remote", remote, "expected", allowedCID)
	}
}
