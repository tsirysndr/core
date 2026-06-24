package microvm

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"sync/atomic"
	"time"

	"gopkg.in/yaml.v3"

	"tangled.org/core/api/tangled"
	"tangled.org/core/log"
	"tangled.org/core/spindle/agentproto"
	agentv1 "tangled.org/core/spindle/agentproto/gen"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

const (
	guestWorkDir          = "/workspace/repo"
	guestBasePATH         = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
	guestDevShellEnvPath  = "/run/spindle/devshell-env.sh"
	activationStepAction  = "activate-config"
	agentAcceptTimeout    = 2 * time.Minute
	agentHandshakeTimeout = 30 * time.Second
	cacheDrainTimeout     = 5 * time.Minute
	vmShutdownTimeout     = 10 * time.Second
	guestTimeoutGrace     = 5 * time.Second
)

type cleanupFunc func(context.Context) error

type Engine struct {
	l            *slog.Logger
	cfg          *config.Config
	db           *db.DB
	agent        *agentHub
	scheduler    *engine.ResourceScheduler[Resources]
	cgroupParent *CgroupParent

	cleanupMu sync.Mutex
	cleanup   map[string][]cleanupFunc
}

type Step struct {
	name        string
	kind        models.StepKind
	command     string
	environment map[string]string
	action      string
	config      manifestConfig
	configKey   string
}

func (s Step) Name() string          { return s.name }
func (s Step) Command() string       { return s.command }
func (s Step) Kind() models.StepKind { return s.kind }

func New(ctx context.Context, cfg *config.Config, d *db.DB) (*Engine, error) {
	l := log.FromContext(ctx).With("component", "engine.microvm")
	port := cfg.MicroVMPipelines.AgentPort
	if port == 0 {
		port = agentproto.DefaultPort
	}
	agent, err := newAgentHub(port, l)
	if err != nil {
		return nil, err
	}
	budget, max, agingThreshold := newVMBudgetConfig(cfg.MicroVMPipelines)
	l.Info("initialized microVM workflow budget", "budget", budget.String(), "maxWorkflow", max.String(), "agingThreshold", agingThreshold)

	var cgroupParent *CgroupParent
	if cfg.MicroVMPipelines.EnableCgroups {
		cgroupParent, err = initCgroupParent(cfg.MicroVMPipelines.CgroupParent, cfg.MicroVMPipelines.CgroupSupervisorMemoryMinMiB, l)
		if err != nil {
			return nil, err
		}
	}

	return &Engine{
		l:            l,
		cfg:          cfg,
		db:           d,
		agent:        agent,
		scheduler:    engine.NewResourceScheduler(budget, max, agingThreshold),
		cgroupParent: cgroupParent,
		cleanup:      make(map[string][]cleanupFunc),
	}, nil
}

func (e *Engine) InitWorkflow(twf tangled.Pipeline_Workflow, tpl tangled.Pipeline) (*models.Workflow, error) {
	swf := &models.Workflow{}
	var dwf manifestWorkflow

	if err := yaml.Unmarshal([]byte(twf.Raw), &dwf); err != nil {
		return nil, err
	}

	for _, dstep := range dwf.Steps {
		swf.Steps = append(swf.Steps, Step{
			name:        dstep.Name,
			kind:        models.StepKindUser,
			command:     dstep.Command,
			environment: dstep.Environment,
		})
	}
	swf.Name = twf.Name
	swf.Environment = dwf.Environment

	if tpl.TriggerMetadata != nil {
		if clone := models.BuildCloneStep(twf, *tpl.TriggerMetadata, e.cfg.Server.Dev); clone.Command() != "" {
			swf.Steps = append([]models.Step{clone}, swf.Steps...)
		}
	}

	imageSpec, imageSpecPath, imageName, err := e.resolveImage(dwf.Image)
	if err != nil {
		return nil, err
	}
	configKey := ""
	config := manifestConfig{
		Services:       dwf.Services,
		Virtualisation: dwf.Virtualisation,
		Dependencies:   dwf.Dependencies,
		Registry:       dwf.Registry,
	}
	if config.Enabled() {
		if !imageSpec.SupportsConfigActivation() {
			return nil, fmt.Errorf(
				"microVM image %q is not a NixOS image: services, virtualisation, dependencies and registry workflow options require a NixOS image",
				imageName,
			)
		}
		var err error
		configKey, err = buildConfigKey(imageSpec, config)
		if err != nil {
			return nil, fmt.Errorf("build config key: %w", err)
		}
		activationStep := Step{
			name:      "NixOS config activation",
			kind:      models.StepKindSystem,
			command:   "activate nixos config",
			action:    activationStepAction,
			config:    config,
			configKey: configKey,
		}

		insertAt := 0
		if len(swf.Steps) > 0 && swf.Steps[0].Kind() == models.StepKindSystem {
			insertAt = 1
		}
		swf.Steps = append(swf.Steps, nil)
		copy(swf.Steps[insertAt+1:], swf.Steps[insertAt:])
		swf.Steps[insertAt] = activationStep
	}

	cacheURLs, cacheKeys, err := workflowCaches(dwf.Caches)
	if err != nil {
		return nil, err
	}

	swf.Data = &workflowState{
		ImageSpec:              imageSpec,
		ImageSpecPath:          imageSpecPath,
		Config:                 config,
		ConfigKey:              configKey,
		Image:                  imageName,
		CacheReadURLs:          cacheURLs,
		CacheTrustedPublicKeys: cacheKeys,
		NixOSToplevelCache:     newNixOSToplevelCacheStore(e.db),
	}
	return swf, nil
}

func (e *Engine) SetupWorkflow(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, wfLogger models.WorkflowLogger) error {
	l := e.l.With("workflow", wid)
	setupStep := Step{name: "microVM setup", kind: models.StepKindSystem}

	wfLogger.ControlWriter(-1, setupStep, models.StepStatusStart).Write([]byte{0})
	defer wfLogger.ControlWriter(-1, setupStep, models.StepStatusEnd).Write([]byte{0})

	state, ok := wf.Data.(*workflowState)
	if !ok || state == nil {
		return fmt.Errorf("workflow state is not initialized")
	}

	cid, err := AllocateCID()
	if err != nil {
		return err
	}
	connCh, unregister, err := e.agent.expect(cid)
	if err != nil {
		return err
	}
	defer unregister()

	workDirBase := e.cfg.MicroVMPipelines.OverlayDir
	if workDirBase == "" {
		workDirBase = os.TempDir()
	}
	workDir, err := os.MkdirTemp(workDirBase, "spindle-microvm-"+wid.String()+"-*")
	if err != nil {
		return fmt.Errorf("create workflow microVM directory: %w", err)
	}
	state.WorkDir = workDir

	setupDone := false
	defer func() {
		if setupDone {
			return
		}
		if detail := vmCrashLog(state.VM); detail != "" {
			l.Error("microVM setup failed", "detail", detail)
		}
		if err := e.cleanupState(context.Background(), wid, state); err != nil {
			l.Error("failed to cleanup failed setup", "error", err)
		}
	}()

	upstreams, err := BuildCacheUpstreams(e.cfg.NixCache.ReadURLs, state.CacheReadURLs)
	if err != nil {
		return err
	}
	readCache, err := StartReadCacheProxy(ctx, cid, upstreams, l)
	if err != nil {
		return err
	}
	state.ReadCache = readCache
	stagingDir := filepath.Join(workDir, "upload-cache")
	uploadCache, err := StartUploadCacheProxy(ctx, cid, e.cfg.NixCache.UploadURL, upstreams, stagingDir, l)
	if err != nil {
		return err
	}
	state.UploadCache = uploadCache
	dnsProxy, err := StartDNSProxy(ctx, cid, l)
	if err != nil {
		return err
	}
	state.DNSProxy = dnsProxy

	port := e.cfg.MicroVMPipelines.AgentPort
	if port == 0 {
		port = agentproto.DefaultPort
	}
	state.ImageSpec.BootArgs = fmt.Sprintf("%s shuttle.vsock_port=%d", state.ImageSpec.BootArgs, port)

	fmt.Fprintf(wfLogger.DataWriter(-1, "stdout"), "starting microVM image %s\n", state.Image)
	l.Info("starting microVM workflow", "image", state.Image, "imageSpec", state.ImageSpecPath, "cid", cid, "workDir", workDir)

	var vm VMHandle
	vm, err = StartVM(ctx, VMConfig{
		Image:     state.ImageSpec,
		CID:       cid,
		EnableKVM: e.cfg.MicroVMPipelines.EnableKVM,
		WorkDir:   workDir,
		Cgroup:    e.cgroupLimits(wid, state.ImageSpec),
		Dev:       e.cfg.Server.Dev,
	}, l)
	if err != nil {
		return err
	}
	state.VM = vm

	acceptCtx, cancelAccept := context.WithTimeout(ctx, agentAcceptTimeout)
	defer cancelAccept()
	conn, err := waitAgentConn(acceptCtx, connCh)
	if err != nil {
		return err
	}

	agentSession := NewAgentSession(conn, l)
	initCtx, cancelInit := context.WithTimeout(ctx, agentHandshakeTimeout)
	defer cancelInit()
	if err := agentSession.Init(initCtx, &agentv1.Init{
		JobId:                  wid.String(),
		CacheTrustedPublicKeys: append(slices.Clone(e.cfg.NixCache.TrustedPublicKeys), state.CacheTrustedPublicKeys...),
		CacheReadProxyPort:     readCache.Port(),
		CacheUploadProxyPort:   uploadCache.Port(),
		DnsProxyPort:           dnsProxy.Port(),
	}); err != nil {
		_ = agentSession.Close()
		return err
	}
	state.Agent = agentSession
	wf.Data = state

	e.registerCleanup(wid, func(ctx context.Context) error {
		return e.cleanupState(ctx, wid, state)
	})
	setupDone = true

	fmt.Fprintf(wfLogger.DataWriter(-1, "stdout"),
		"agent connected; serial log: %s\n", vm.Logs().Serial,
	)
	return nil
}

func applyDepsSource(command string) string {
	return fmt.Sprintf(
		// check if it exists because not all images have this
		`if [ -f %s ]; then . %s; export PATH="$PATH:%s"; fi; %s`,
		guestDevShellEnvPath, guestDevShellEnvPath, guestBasePATH, command,
	)
}

func (e *Engine) RunStep(ctx context.Context, wid models.WorkflowId, w *models.Workflow, idx int, secrets []secrets.UnlockedSecret, wfLogger models.WorkflowLogger) error {
	state, ok := w.Data.(*workflowState)
	if !ok || state == nil || state.Agent == nil {
		return fmt.Errorf("microVM workflow is not connected to agent")
	}

	stderr := wfLogger.DataWriter(idx, "stderr")

	execCtx, vmExited, cancelWatch := watchVMExit(ctx, state.VM)
	defer cancelWatch()

	step := w.Steps[idx]
	if s, ok := step.(Step); ok && s.action == activationStepAction {
		err := e.activateConfig(execCtx, wid, state, s, wfLogger.DataWriter(idx, "stdout"))
		return e.classifyStepError(ctx, wid, step, state, stderr, vmExited, err)
	}
	env := []string{
		"HOME=/workspace",
		"LOGNAME=" + guestWorkflowUser,
		"PATH=" + guestBasePATH,
		"USER=" + guestWorkflowUser,
	}
	for k, v := range w.Environment {
		env = append(env, k+"="+v)
	}
	for _, s := range secrets {
		env = append(env, s.Key+"="+s.Value)
	}
	if s, ok := step.(Step); ok {
		for k, v := range s.environment {
			env = append(env, k+"="+v)
		}
	}

	stdout := wfLogger.DataWriter(idx, "stdout")
	exitCode, err := state.Agent.Exec(execCtx, AgentExec{
		ID: fmt.Sprintf("%s-%d", wid.String(), idx),
		ExecStart: &agentv1.ExecStart{
			Argv: []string{state.ImageSpec.Shell, "-lc", applyDepsSource(step.Command())},
			Env:  env,
			Cwd:  guestWorkDir,
			User: guestWorkflowUser,
			// timeout not set here, Exec will fill it
		},
		Stdout: stdout,
		Stderr: stderr,
	})
	if err != nil {
		return e.classifyStepError(ctx, wid, step, state, stderr, vmExited, err)
	}

	if exitCode != 0 {
		e.l.Debug("step exited non-zero", "workflow", wid, "step", step.Name(), "exitCode", exitCode)
		return engine.ErrWorkflowFailed
	}
	return nil
}

// reads the vm serial logs so we report the tail of that as an error instead of
// just "guest agent connection lost: EOF"
func (e *Engine) classifyStepError(ctx context.Context, wid models.WorkflowId, step models.Step, state *workflowState, stderr io.Writer, vmExited *atomic.Bool, err error) error {
	if err == nil {
		return nil
	}
	l := e.l.With("workflow", wid, "step", step.Name())

	if vmExited != nil && vmExited.Load() {
		reason := "microVM exited unexpectedly"
		oom := state.VM != nil && state.VM.OOMKilled()
		if oom {
			reason = "microVM killed by OOM (cgroup memory limit exceeded)"
		}
		if detail := vmCrashLog(state.VM); detail != "" {
			fmt.Fprintf(stderr, "%s:\n%s\n", reason, detail)
			l.Error(reason, "oom", oom, "detail", detail)
		} else {
			fmt.Fprintln(stderr, reason)
			l.Error(reason, "oom", oom)
		}
		return errors.New(reason + "; see workflow logs for serial output")
	}

	if errors.Is(err, errGuestTimedOut) || ctx.Err() != nil {
		l.Debug("step timed out", "guestReported", errors.Is(err, errGuestTimedOut))
		return engine.ErrTimedOut
	}

	// the agent connection dropped while qemu stayed up (eg. the guest kernel
	// OOM-killed the agent or a guest panic), so surface serial logs, those
	// will be more helpful.
	if detail := vmCrashLog(state.VM); detail != "" {
		fmt.Fprintf(stderr, "step failed (%v):\n%s\n", err, detail)
		l.Error("step failed", "error", err, "detail", detail)
	} else {
		l.Error("step failed", "error", err)
	}
	return err
}

func (e *Engine) activateConfig(ctx context.Context, wid models.WorkflowId, state *workflowState, step Step, out io.Writer) error {
	cfg := step.config
	if !cfg.Enabled() {
		return nil
	}

	configKey := step.configKey
	if configKey == "" {
		configKey = state.ConfigKey
	}

	userConfigJSON, err := json.Marshal(cfg)
	if err != nil {
		return fmt.Errorf("encode user config: %w", err)
	}

	var cachedToplevel string
	if configKey != "" {
		if record, ok, err := state.NixOSToplevelCache.Lookup(configKey); err != nil {
			return err
		} else if ok {
			// todo(dawn): we should probably use gc roots to eliminate TOCTOU
			// the spindle will have to manage the gc roots, and for remote we have to
			// ssh in to the host and add / remove gc root.
			// we need to have this check anyway since the only check http caches can
			// use is this one, since we cant manage gc roots there...
			if e.anyCacheHasPath(ctx, state, record.Toplevel) {
				cachedToplevel = record.Toplevel
				fmt.Fprintf(out, "realizing cached NixOS config %s\n", cachedToplevel)
			}
		}
	}
	if cachedToplevel == "" {
		fmt.Fprintf(out, "building NixOS config from user config\n")
	}

	baseHash, err := BaseConfigHash(state.ImageSpec)
	if err != nil {
		return fmt.Errorf("calculate base config hash: %w", err)
	}

	result, err := state.Agent.ActivateConfig(ctx, fmt.Sprintf("%s-config", wid.String()), &agentv1.ActivateConfig{
		ConfigKey:      configKey,
		BaseConfigHash: baseHash,
		UserConfig:     string(userConfigJSON),
		Toplevel:       cachedToplevel,
	}, out)
	if err != nil {
		return err
	}
	fmt.Fprintf(out, "activated NixOS config toplevel %s\n", result.Toplevel)

	if cachedToplevel != "" || configKey == "" {
		return nil
	}
	if e.cfg.NixCache.UploadURL == "" {
		e.l.Warn("not committing config cache metadata: no upload URL configured", "workflow", wid, "configKey", configKey, "toplevel", result.Toplevel)
		return nil
	}

	if err := e.drainNixCache(ctx, state); err != nil {
		// a partial upload would leave the cache unable to realize this toplevel,
		// so skip the metadata commit rather than poison it with an un-realizable
		// key. the config still activated fine, so don't fail the workflow.
		e.l.Warn("cache drain failed; skipping config cache metadata commit", "workflow", wid, "configKey", configKey, "toplevel", result.Toplevel, "error", err)
		return nil
	}
	if err := state.NixOSToplevelCache.Commit(configKey, result.Toplevel); err != nil {
		return err
	}
	fmt.Fprintf(out, "committed config cache metadata %s -> %s\n", configKey, result.Toplevel)
	return nil
}

func (e *Engine) anyCacheHasPath(ctx context.Context, state *workflowState, storePath string) bool {
	upstreams, err := BuildCacheUpstreams(e.cfg.NixCache.ReadURLs, state.CacheReadURLs)
	if err != nil {
		e.l.Warn("config cache check: build upstreams failed; treating as absent", "path", storePath, "error", err)
		return false
	}
	if len(upstreams) == 0 {
		return false
	}
	hash, _, err := parseStorePath(storePath)
	if err != nil {
		e.l.Warn("config cache check: invalid toplevel path; treating as absent", "path", storePath, "error", err)
		return false
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodHead, "http://upstream/"+hash+".narinfo", nil)
	if err != nil {
		e.l.Warn("config cache check: build request failed; treating as absent", "path", storePath, "error", err)
		return false
	}
	resp, err := newNarinfoExistenceTransport(upstreams, e.l).RoundTrip(req)
	if err != nil {
		e.l.Warn("config cache check: narinfo probe failed; treating as absent", "path", storePath, "error", err)
		return false
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, resp.Body)
	return resp.StatusCode == http.StatusOK
}

func (e *Engine) DestroyWorkflow(ctx context.Context, wid models.WorkflowId) error {
	fns := e.drainCleanups(wid)

	var cleanupErr error
	for i := len(fns) - 1; i >= 0; i-- {
		if err := fns[i](ctx); err != nil {
			e.l.Error("failed to cleanup workflow resource", "workflowId", wid, "error", err)
			cleanupErr = errors.Join(cleanupErr, err)
		}
	}
	return cleanupErr
}

func (e *Engine) FinalizeWorkflow(ctx context.Context, wid models.WorkflowId, w *models.Workflow, wfLogger models.WorkflowLogger) error {
	return nil
}

func (e *Engine) WorkflowTimeout() time.Duration {
	d, err := time.ParseDuration(e.cfg.MicroVMPipelines.WorkflowTimeout)
	if err != nil {
		d = 5 * time.Minute
	}
	return d + guestTimeoutGrace
}

func (e *Engine) registerCleanup(wid models.WorkflowId, fn cleanupFunc) {
	e.cleanupMu.Lock()
	defer e.cleanupMu.Unlock()
	key := wid.String()
	e.cleanup[key] = append(e.cleanup[key], fn)
}

func (e *Engine) drainCleanups(wid models.WorkflowId) []cleanupFunc {
	e.cleanupMu.Lock()
	defer e.cleanupMu.Unlock()
	key := wid.String()
	fns := e.cleanup[key]
	delete(e.cleanup, key)
	return fns
}

func (e *Engine) cgroupLimits(wid models.WorkflowId, spec ImageSpec) CgroupLimits {
	cfg := e.cfg.MicroVMPipelines
	return CgroupLimits{
		Enabled:      cfg.EnableCgroups,
		Parent:       e.cgroupParent,
		Name:         "workflow-" + wid.String(),
		MemoryMaxMiB: resourcesForImage(spec).MemoryMiB,
		SwapMaxMiB:   cfg.CgroupSwapMaxMiB,
		PidsMax:      cfg.CgroupPidsMax,
	}
}
