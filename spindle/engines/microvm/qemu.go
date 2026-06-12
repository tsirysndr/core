package microvm

import (
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/digitalocean/go-qemu/qmp"
	"github.com/google/uuid"
)

const (
	defaultQMPTimeout = 10 * time.Second
	outerSlirpCIDR    = "10.0.2.0/24"
	innerSlirpNet     = "10.0.3.0/24"
	innerSlirpHost    = "10.0.3.2"
	innerSlirpDNS     = "10.0.3.3"
	innerSlirpDHCP    = "10.0.3.15"
	netnsTapName      = "tap0"
	netnsMTU          = "65520"
)

type QEMUConfig struct {
	Image          ImageSpec
	BootTimeout    time.Duration
	CID            uint32
	EnableKVM      bool
	QEMULogPath    string
	QMPPath        string
	SerialLogPath  string
	WorkDir        string
	VolumePaths    map[string]string
	VolumeBaseName string
	Cgroup         CgroupLimits
	Dev            bool
}

type QEMUVMHandle struct {
	cid           uint32
	Process       *os.Process
	qemuLogPath   string
	QMPMon        *qmp.SocketMonitor
	QMPPath       string
	serialLogPath string
	workDir       string

	cmd         *exec.Cmd
	done        chan struct{}
	qemuLogFile *os.File
	cgroup      *CgroupHandle
	slirpCmd    *exec.Cmd
	slirpExit   *os.File
	waitErr     error
	waitErrMu   sync.Mutex
}

type qemuRunner struct{}

func (qemuRunner) Validate(spec ImageSpec, enableKVM bool) error {
	if _, err := exec.LookPath(spec.RunnerCmd()); err != nil {
		return fmt.Errorf("required host command %q not found in PATH: %w", spec.RunnerCmd(), err)
	}
	if _, err := os.Stat("/dev/vhost-vsock"); err != nil {
		return fmt.Errorf("microvm requires /dev/vhost-vsock for vhost-vsock-device: %w", err)
	}
	if enableKVM {
		if _, err := os.Stat("/dev/kvm"); err != nil {
			return fmt.Errorf("microvm KVM was requested but /dev/kvm is not accessible: %w", err)
		}
	}
	if len(spec.NetworkInterfaces) > 0 {
		if _, err := os.Stat("/dev/net/tun"); err != nil {
			return fmt.Errorf("microvm slirp4netns networking requires /dev/net/tun: %w", err)
		}
		for _, cmd := range []string{"ip", "mount", "slirp4netns", "unshare"} {
			if _, err := exec.LookPath(cmd); err != nil {
				return fmt.Errorf("required host command %q not found in PATH: %w", cmd, err)
			}
		}
	}
	return nil
}

func (qemuRunner) Start(ctx context.Context, cfg VMConfig, volumePaths map[string]string, logger *slog.Logger) (VMHandle, error) {
	bootTimeout := cfg.BootTimeout
	if bootTimeout == 0 {
		bootTimeout = 10 * time.Second
	}
	return StartQEMU(ctx, QEMUConfig{
		Image:       cfg.Image,
		BootTimeout: bootTimeout,
		CID:         cfg.CID,
		EnableKVM:   cfg.EnableKVM,
		WorkDir:     cfg.WorkDir,
		VolumePaths: volumePaths,
		Cgroup:      cfg.Cgroup,
		Dev:         cfg.Dev,
	}, logger)
}

func StartQEMU(ctx context.Context, cfg QEMUConfig, logger *slog.Logger) (VMHandle, error) {
	if logger == nil {
		logger = slog.Default()
	}

	workDir := cfg.WorkDir

	handle := &QEMUVMHandle{
		workDir: workDir,
	}

	var ok bool
	defer func() {
		if !ok {
			_ = handle.Close()
		}
	}()

	cid := cfg.CID
	if cid == 0 {
		var err error
		cid, err = AllocateCID()
		if err != nil {
			return nil, err
		}
	}
	if cid < minGuestCID {
		return nil, fmt.Errorf("guest CID must be >= %d", minGuestCID)
	}
	handle.cid = cid

	volumePaths := cfg.VolumePaths

	qemuLogPath := cfg.QEMULogPath
	if qemuLogPath == "" {
		qemuLogPath = filepath.Join(workDir, "qemu.log")
	}
	qemuLogFile, err := createParentedFile(qemuLogPath)
	if err != nil {
		return nil, err
	}
	handle.qemuLogPath = qemuLogPath
	handle.qemuLogFile = qemuLogFile

	serialLogPath := cfg.SerialLogPath
	if serialLogPath == "" {
		serialLogPath = filepath.Join(workDir, "serial.log")
	}
	if err := os.MkdirAll(filepath.Dir(serialLogPath), 0o755); err != nil {
		return nil, fmt.Errorf("create serial log directory: %w", err)
	}
	handle.serialLogPath = serialLogPath

	qmpPath := cfg.QMPPath
	if qmpPath == "" {
		qmpPath = filepath.Join(workDir, "qmp.sock")
	}
	handle.QMPPath = qmpPath

	qemuCmd := cfg.Image.RunnerCmd()
	qemuBinary, err := exec.LookPath(qemuCmd)
	if err != nil {
		return nil, fmt.Errorf("%s command not found in PATH: %w", qemuCmd, err)
	}

	args, err := qemuArgs(qemuArgsConfig{
		Image:         cfg.Image,
		CID:           cid,
		EnableKVM:     cfg.EnableKVM,
		QMPPath:       qmpPath,
		SerialLogPath: serialLogPath,
		VolumePaths:   volumePaths,
	})
	if err != nil {
		return nil, err
	}

	cmd, slirpNet, err := qemuCommand(ctx, qemuBinary, args, cfg.Image, workDir, cfg.Dev)
	if err != nil {
		return nil, err
	}
	cmd.Env = append(os.Environ(), "TMPDIR="+workDir)
	cmd.Stdout = qemuLogFile
	cmd.Stderr = qemuLogFile

	cgroup, err := prepareCgroup(cfg.Cgroup, logger)
	if err != nil {
		return nil, err
	}
	handle.cgroup = cgroup

	logger.Info("starting qemu microvm", "cid", cid, "workDir", workDir, "serialLog", serialLogPath, "qmp", qmpPath)
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("starting qemu: %w", err)
	}
	handle.cmd = cmd
	handle.Process = cmd.Process
	handle.done = make(chan struct{})
	go func() {
		err := cmd.Wait()
		handle.waitErrMu.Lock()
		handle.waitErr = err
		handle.waitErrMu.Unlock()
		close(handle.done)
	}()

	if err := cgroup.AddProcess(cmd.Process.Pid, logger); err != nil {
		return nil, err
	}

	if slirpNet != nil {
		handle.slirpCmd, handle.slirpExit, err = slirpNet.Start(ctx, qemuLogFile, logger)
		if err != nil {
			return nil, err
		}
		if handle.slirpCmd != nil && handle.slirpCmd.Process != nil {
			if err := cgroup.AddProcess(handle.slirpCmd.Process.Pid, logger); err != nil {
				return nil, err
			}
		}
	}

	qmpTimeout := cfg.BootTimeout
	if qmpTimeout == 0 {
		qmpTimeout = defaultQMPTimeout
	}
	if err := handle.waitForQMP(ctx, qmpTimeout); err != nil {
		return nil, err
	}

	status, err := handle.QMPQueryStatus()
	if err != nil {
		return nil, err
	}
	if status != "running" {
		return nil, fmt.Errorf("qemu guest not running (status: %s)", status)
	}
	logger.Info("qemu microvm running", "cid", cid, "status", status)

	ok = true
	return handle, nil
}

func (h *QEMUVMHandle) Wait() error {
	if h == nil || h.done == nil {
		return nil
	}
	<-h.done
	h.waitErrMu.Lock()
	defer h.waitErrMu.Unlock()
	return h.waitErr
}

func (h *QEMUVMHandle) WaitContext(ctx context.Context) error {
	if h == nil || h.done == nil {
		return nil
	}
	select {
	case <-h.done:
		h.waitErrMu.Lock()
		defer h.waitErrMu.Unlock()
		return h.waitErr
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (h *QEMUVMHandle) Kill() error {
	if h == nil || h.Process == nil {
		return nil
	}
	return h.Process.Kill()
}

func (h *QEMUVMHandle) Shutdown(ctx context.Context) error {
	if h == nil {
		return nil
	}
	if h.QMPMon != nil {
		if err := h.QMPSystemPowerdown(); err != nil {
			return err
		}
	}
	if h.done == nil {
		return nil
	}
	select {
	case <-h.done:
		return h.Wait()
	case <-ctx.Done():
		_ = h.Kill()
		_ = h.Wait()
		return ctx.Err()
	}
}

func (h *QEMUVMHandle) Close() error {
	if h == nil {
		return nil
	}

	var closeErr error
	if h.QMPMon != nil {
		closeErr = errors.Join(closeErr, h.QMPMon.Disconnect())
		h.QMPMon = nil
	}
	if h.Process != nil {
		_ = h.Process.Kill()
		_ = h.Wait()
	}
	if h.slirpExit != nil {
		_ = h.slirpExit.Close()
		h.slirpExit = nil
	}
	if h.slirpCmd != nil && h.slirpCmd.Process != nil {
		_ = h.slirpCmd.Process.Kill()
		_ = h.slirpCmd.Wait()
		h.slirpCmd = nil
	}
	if h.qemuLogFile != nil {
		closeErr = errors.Join(closeErr, h.qemuLogFile.Close())
		h.qemuLogFile = nil
	}
	if h.cgroup != nil {
		closeErr = errors.Join(closeErr, h.cgroup.Close())
		h.cgroup = nil
	}
	return closeErr
}

func (h *QEMUVMHandle) QMPRun(command qmp.Command) ([]byte, error) {
	if h == nil || h.QMPMon == nil {
		return nil, fmt.Errorf("qmp monitor is not connected")
	}
	data, err := json.Marshal(command)
	if err != nil {
		return nil, err
	}
	return h.QMPMon.Run(data)
}

func (h *QEMUVMHandle) QMPQueryStatus() (string, error) {
	raw, err := h.QMPRun(qmp.Command{Execute: "query-status"})
	if err != nil {
		return "", fmt.Errorf("qmp query-status failed: %w", err)
	}

	var resp struct {
		Return struct {
			Status string `json:"status"`
		} `json:"return"`
	}
	if err := json.Unmarshal(raw, &resp); err != nil {
		return "", fmt.Errorf("qmp query-status parse: %w", err)
	}
	return resp.Return.Status, nil
}

func (h *QEMUVMHandle) QMPSystemPowerdown() error {
	_, err := h.QMPRun(qmp.Command{Execute: "system_powerdown"})
	return err
}

func (h *QEMUVMHandle) Logs() VMLogs {
	if h == nil {
		return VMLogs{}
	}
	return VMLogs{
		Serial: h.serialLogPath,
		Extra: map[string]string{
			"qemu": h.qemuLogPath,
		},
	}
}

func (h *QEMUVMHandle) CID() uint32 {
	if h == nil {
		return 0
	}
	return h.cid
}

func (h *QEMUVMHandle) WorkDir() string {
	if h == nil {
		return ""
	}
	return h.workDir
}

func (h *QEMUVMHandle) OOMKilled() bool {
	if h == nil {
		return false
	}
	return h.cgroup.OOMKilled()
}

func (h *QEMUVMHandle) waitForQMP(ctx context.Context, timeout time.Duration) error {
	qmpCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	var lastErr error
	for {
		mon, err := qmp.NewSocketMonitor("unix", h.QMPPath, 2*time.Second)
		if err == nil {
			if err = mon.Connect(); err == nil {
				h.QMPMon = mon
				return nil
			}
			_ = mon.Disconnect()
		}
		lastErr = err

		select {
		case <-qmpCtx.Done():
			return fmt.Errorf("qmp connect timeout: %w", lastErr)
		case <-h.done:
			return fmt.Errorf("qemu exited before qmp was ready: %w", h.Wait())
		case <-time.After(25 * time.Millisecond):
		}
	}
}

func qemuCommand(
	ctx context.Context,
	qemuBinary string,
	args []string,
	spec ImageSpec,
	workDir string,
	dev bool,
) (*exec.Cmd, *slirpNamespace, error) {
	if len(spec.NetworkInterfaces) == 0 {
		return exec.CommandContext(ctx, qemuBinary, args...), nil, nil
	}

	ipPath, err := exec.LookPath("ip")
	if err != nil {
		return nil, nil, fmt.Errorf("ip command not found in PATH: %w", err)
	}
	mountPath, err := exec.LookPath("mount")
	if err != nil {
		return nil, nil, fmt.Errorf("mount command not found in PATH: %w", err)
	}
	unsharePath, err := exec.LookPath("unshare")
	if err != nil {
		return nil, nil, fmt.Errorf("unshare command not found in PATH: %w", err)
	}

	pidFile, resolvPath, wrapperPath, err := prepareQEMUNetnsFiles(workDir, dev)
	if err != nil {
		return nil, nil, err
	}

	cmdArgs := append([]string{
		"--user",
		"--map-root-user",
		"--net",
		"--mount",
		"--propagation", "private",
		"--",
		wrapperPath,
		pidFile,
		ipPath,
		mountPath,
		resolvPath,
		qemuBinary,
	}, args...)

	cmd := exec.CommandContext(ctx, unsharePath, cmdArgs...)

	return cmd, &slirpNamespace{
		spec:    spec,
		pidFile: pidFile,
		dev:     dev,
	}, nil
}

func prepareQEMUNetnsFiles(workDir string, dev bool) (pidFile, resolvPath, wrapperPath string, err error) {
	pidFile = filepath.Join(workDir, "qemu-netns.pid")
	resolvPath = filepath.Join(workDir, "qemu-netns-resolv.conf")
	wrapperPath = filepath.Join(workDir, "qemu-netns-wrapper")

	// the guest resolves through shuttle on 127.0.0.1. keep qemu's slirp DNS
	// pointed at an unroutable local resolver inside this network namespace so
	// direct guest queries to 10.0.3.3 don't bypass the shuttle dns policy.
	if err := os.WriteFile(resolvPath, []byte("nameserver 127.0.0.1\n"), 0o644); err != nil {
		return "", "", "", fmt.Errorf("write qemu network namespace resolv.conf: %w", err)
	}

	if err := writeNetnsWrapper(wrapperPath, dev); err != nil {
		return "", "", "", fmt.Errorf("write qemu network namespace wrapper: %w", err)
	}

	return pidFile, resolvPath, wrapperPath, nil
}

type qemuArgsConfig struct {
	Image         ImageSpec
	CID           uint32
	EnableKVM     bool
	QMPPath       string
	SerialLogPath string
	VolumePaths   map[string]string
}

func qemuArgs(cfg qemuArgsConfig) ([]string, error) {
	uuid := uuid.New()

	b := newArgBuilder(64)

	addQEMUMachineArgs(&b, cfg, uuid)
	addQEMUStoreArgs(&b, cfg)

	if cfg.EnableKVM {
		addQEMUKVMArgs(&b, cfg.Image)
	}

	if err := addQEMUVolumeArgs(&b, cfg); err != nil {
		return nil, err
	}

	if err := addQEMUNetworkArgs(&b, cfg.Image.NetworkInterfaces); err != nil {
		return nil, err
	}

	b.Optf("-device", "vhost-vsock-device,guest-cid=%d", cfg.CID)

	if len(cfg.Image.RunnerConfig.ExtraArgs) > 0 {
		b.Add(cfg.Image.RunnerConfig.ExtraArgs...)
	}

	return b.Args(), nil
}

func addQEMUMachineArgs(b *argBuilder, cfg qemuArgsConfig, uuid uuid.UUID) {
	if cfg.Image.RunnerConfig.Machine != "" {
		b.Opt("-M", cfg.Image.RunnerConfig.Machine)
	}
	b.Optf("-m", "%dM", cfg.Image.MemoryMiB)
	b.Opt("-smp", strconv.Itoa(cfg.Image.VCPUs))

	b.Add(
		"-nodefaults",
		"-no-user-config",
		"-no-reboot",
	)

	b.Opt("-kernel", cfg.Image.Kernel)
	b.Opt("-initrd", cfg.Image.Initrd)

	b.Opt("-device", "virtio-rng-device")

	b.Optf("-smbios", "type=1,uuid=%s", uuid)
	b.Opt("-serial", "file:"+cfg.SerialLogPath)

	// use virtio console if requsted. this is faster than the serial UART logging
	// because serial has a higher cost when being accesssed. we still have to
	// support serial itself for early kernel boot but thats OK.
	if cfg.Image.RunnerConfig.Console == "hvc0" {
		b.Optf("-chardev", "file,id=virtiocon0,path=%s,append=on", cfg.SerialLogPath)
		b.Add("-device", "virtio-serial-device")
		b.Opt("-device", "virtconsole,chardev=virtiocon0")
	}
	b.Opt("-display", "none")
	b.Opt("-monitor", "none")
	b.Opt("-append", cfg.Image.BootArgs)

	b.Opt("-sandbox", "on")
	b.Optf("-qmp", "unix:%s,server,nowait", cfg.QMPPath)
}

func addQEMUStoreArgs(b *argBuilder, cfg qemuArgsConfig) {
	drive := newOptionBuilder(8)
	drive.KV("id", "store")
	drive.KV("format", "raw")
	drive.Add("read-only=on")
	drive.KV("file", cfg.Image.StoreDisk)
	drive.Add("if=none")
	drive.Add("aio=io_uring")

	b.Opt("-drive", drive.String())
	b.Opt("-device", "virtio-blk-device,drive=store")
}

func addQEMUKVMArgs(b *argBuilder, image ImageSpec) {
	b.Flag("-enable-kvm")
	if image.RunnerConfig.CPU != "" {
		b.Opt("-cpu", image.RunnerConfig.CPU)
	}
	b.Opt("-device", "i8042")
}

func addQEMUVolumeArgs(b *argBuilder, cfg qemuArgsConfig) error {
	for index, volume := range cfg.Image.Volumes {
		path := cfg.VolumePaths[volume.Image]
		if path == "" {
			return fmt.Errorf("missing prepared path for volume %q", volume.Image)
		}

		driveID := fmt.Sprintf("volume%d", index)

		drive := newOptionBuilder(10)
		drive.KV("id", driveID)
		drive.KV("format", "raw")
		drive.Add("read-only=off")
		drive.KV("file", path)
		drive.Add("if=none")
		drive.Add("aio=io_uring")
		drive.Add("discard=unmap")
		drive.Add("cache=none")

		b.Opt("-drive", drive.String())
		b.Optf("-device", "virtio-blk-device,drive=%s", driveID)
	}

	return nil
}

func addQEMUNetworkArgs(b *argBuilder, interfaces []NetworkInterface) error {
	for _, networkInterface := range interfaces {
		if networkInterface.Type != "slirp4netns" {
			return fmt.Errorf("unsupported microvm network interface type %q", networkInterface.Type)
		}

		netdevOpts := newOptionBuilder(6)
		netdevOpts.Add("user")
		netdevOpts.KV("id", networkInterface.ID)
		netdevOpts.KV("net", innerSlirpNet)
		netdevOpts.KV("host", innerSlirpHost)
		netdevOpts.KV("dns", innerSlirpDNS)
		netdevOpts.KV("dhcpstart", innerSlirpDHCP)

		b.Opt("-netdev", netdevOpts.String())
		b.Optf(
			"-device", "virtio-net-device,netdev=%s,mac=%s",
			networkInterface.ID, networkInterface.MAC,
		)
	}

	return nil
}

func waitForPIDFile(ctx context.Context, path string) (string, error) {
	waitCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()

	ticker := time.NewTicker(25 * time.Millisecond)
	defer ticker.Stop()

	for {
		data, err := os.ReadFile(path)
		if err == nil {
			pid := strings.TrimSpace(string(data))
			if pid != "" {
				return pid, nil
			}
		} else if !errors.Is(err, os.ErrNotExist) {
			return "", fmt.Errorf("read qemu network namespace pid: %w", err)
		}

		select {
		case <-waitCtx.Done():
			return "", fmt.Errorf("waiting for qemu network namespace pid: %w", waitCtx.Err())
		case <-ticker.C:
		}
	}
}
