package dagger

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"path"
	"runtime"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/image"
	"github.com/docker/docker/api/types/mount"
	"github.com/docker/docker/api/types/network"
	"github.com/docker/docker/client"
	"github.com/docker/docker/pkg/stdcopy"
	"gopkg.in/yaml.v3"
	"tangled.org/core/api/tangled"
	"tangled.org/core/log"
	"tangled.org/core/spindle/config"
	"tangled.org/core/spindle/engine"
	"tangled.org/core/spindle/models"
	"tangled.org/core/spindle/secrets"
)

const (
	workspaceDir = "/tangled/workspace"
	homeDir      = "/tangled/home"

	daggerDir = "/tangled/dagger"
	shimDir   = daggerDir + "/bin"
	cliDir    = daggerDir + "/cli"

	moduleEnv  = "TANGLED_DAGGER_MODULE"
	versionEnv = "TANGLED_DAGGER_VERSION"
	dirEnv     = "TANGLED_DAGGER_DIR"

	defaultPackages = "bash git coreutils curl gnutar gzip docker-client"
)

type cleanupFunc func(context.Context) error

type Engine struct {
	dockerMu sync.Mutex
	docker   client.APIClient
	l        *slog.Logger
	cfg      *config.Config

	slotter engine.WorkflowSlotter

	cleanupMu sync.Mutex
	cleanup   map[string][]cleanupFunc
}

type Step struct {
	name        string
	kind        models.StepKind
	command     string
	environment map[string]string
}

func (s Step) Name() string {
	return s.name
}

func (s Step) Command() string {
	return s.command
}

func (s Step) Kind() models.StepKind {
	return s.kind
}

type setupSteps []models.Step

func (ss *setupSteps) addStep(step models.Step) {
	*ss = append(*ss, step)
}

type addlFields struct {
	image     string
	container string
	mounts    []mount.Mount
	module    string
	version   string
}

func New(ctx context.Context, cfg *config.Config) (*Engine, error) {
	l := log.FromContext(ctx).With("component", "spindle", "engine", "dagger")

	return &Engine{
		l:       l,
		cfg:     cfg,
		slotter: engine.NewSemaphoreSlotter(cfg.DaggerPipelines.MaxConcurrentWorkflows),
		cleanup: make(map[string][]cleanupFunc),
	}, nil
}

func (e *Engine) InitWorkflow(twf tangled.Pipeline_Workflow, tpl tangled.Pipeline) (*models.Workflow, error) {
	swf := &models.Workflow{}
	addl := addlFields{}

	dwf := &struct {
		Steps []struct {
			Command     string            `yaml:"command"`
			Name        string            `yaml:"name"`
			Environment map[string]string `yaml:"environment"`
		} `yaml:"steps"`
		Module       string            `yaml:"module"`
		Version      string            `yaml:"version"`
		Dependencies []string          `yaml:"dependencies"`
		Environment  map[string]string `yaml:"environment"`
	}{}
	if err := engine.DescribeManifestError(twf.Raw, dwf); err != nil {
		return nil, err
	}
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

	addl.module = dwf.Module
	addl.version = dwf.Version
	if addl.version == "" {
		addl.version = e.cfg.DaggerPipelines.Version
	}
	addl.image = e.workflowImage(dwf.Dependencies)

	if sock := e.cfg.Server.DockerSocket; sock != "" {
		addl.mounts = append(addl.mounts, mount.Mount{
			Type:     mount.TypeBind,
			Source:   sock,
			Target:   sock,
			ReadOnly: false,
		})
	}

	setup := &setupSteps{}
	if tpl.TriggerMetadata != nil {
		setup.addStep(models.BuildCloneStep(twf, *tpl.TriggerMetadata, e.cfg.Server.Dev))
	}
	setup.addStep(installDaggerStep())
	setup.addStep(detectModuleStep())
	setup.addStep(linkFunctionsStep())

	swf.Steps = append(*setup, swf.Steps...)
	swf.Data = addl

	return swf, nil
}

func (e *Engine) workflowImage(deps []string) string {
	if img := e.cfg.DaggerPipelines.Image; img != "" {
		return img
	}

	var packages []string
	for _, p := range append(strings.Fields(defaultPackages), deps...) {
		if p == "" || slices.Contains(packages, p) {
			continue
		}
		packages = append(packages, p)
	}

	joined := path.Join(packages...)
	if runtime.GOARCH == "arm64" {
		joined = path.Join("arm64", joined)
	}

	return path.Join(e.cfg.DaggerPipelines.Nixery, joined)
}

func (e *Engine) WorkflowTimeout() time.Duration {
	workflowTimeoutStr := e.cfg.DaggerPipelines.WorkflowTimeout
	workflowTimeout, err := time.ParseDuration(workflowTimeoutStr)
	if err != nil {
		e.l.Error("failed to parse workflow timeout", "error", err, "timeout", workflowTimeoutStr)
		workflowTimeout = 15 * time.Minute
	}

	return workflowTimeout
}

func (e *Engine) ensureDocker() (client.APIClient, error) {
	e.dockerMu.Lock()
	defer e.dockerMu.Unlock()

	if e.docker != nil {
		return e.docker, nil
	}

	dcli, err := client.NewClientWithOpts(client.FromEnv, client.WithAPIVersionNegotiation())
	if err != nil {
		return nil, err
	}
	e.docker = dcli
	return dcli, nil
}

func (e *Engine) AcquireWorkflowSlot(
	ctx context.Context,
	wid models.WorkflowId,
	wf *models.Workflow,
	mode engine.AcquireMode,
) (engine.WorkflowSlot, error) {
	if e.slotter == nil {
		return engine.NoopSlot{}, nil
	}

	return e.slotter.AcquireWorkflowSlot(ctx, wid, wf, mode)
}

func (e *Engine) SetupWorkflow(ctx context.Context, wid models.WorkflowId, wf *models.Workflow, wfLogger models.WorkflowLogger) (err error) {
	l := e.l.With("workflow", wid)
	l.Info("setting up workflow")

	setupStep := Step{
		name: "Prepare Dagger container",
		kind: models.StepKindSystem,
	}
	setupStepIdx := -1

	wfLogger.ControlWriter(setupStepIdx, setupStep, models.StepStatusStart).Write([]byte{0})
	defer wfLogger.ControlWriter(setupStepIdx, setupStep, models.StepStatusEnd).Write([]byte{0})

	defer func() {
		if err != nil {
			err = fmt.Errorf("Failed to setup container:\n%w", err)
		}
	}()

	if e.cfg.DaggerPipelines.RunnerHost == "" && e.cfg.Server.DockerSocket == "" {
		return ErrNoRunner
	}

	if _, err := e.ensureDocker(); err != nil {
		return err
	}

	_, err = e.docker.NetworkCreate(ctx, networkName(wid), network.CreateOptions{
		Driver: "bridge",
	})
	if err != nil {
		return err
	}

	e.registerCleanup(wid, func(ctx context.Context) error {
		if err := e.docker.NetworkRemove(ctx, networkName(wid)); err != nil {
			return fmt.Errorf("removing network: %w", err)
		}
		return nil
	})

	addl := wf.Data.(addlFields)
	l.Info("pulling image", "image", addl.image)
	fmt.Fprintf(
		wfLogger.DataWriter(setupStepIdx, "stdout"),
		"Pulling image: %s",
		addl.image,
	)

	reader, err := e.docker.ImagePull(ctx, addl.image, image.PullOptions{})
	if err != nil {
		l.Error("pipeline image pull failed!", "error", err.Error())
		fmt.Fprintf(wfLogger.DataWriter(setupStepIdx, "stderr"), "image pull failed: %s", err)
		return fmt.Errorf("pulling image: %w", err)
	}
	defer reader.Close()

	scanner := bufio.NewScanner(reader)
	for scanner.Scan() {
		line := scanner.Text()
		wfLogger.DataWriter(setupStepIdx, "stdout").Write([]byte(line))
		l.Info("image pull progress", "stdout", line)
	}

	l.Info("creating container")
	wfLogger.DataWriter(setupStepIdx, "stdout").Write([]byte("Creating container..."))

	extraHosts := []string{"host.docker.internal:host-gateway"}
	for _, h := range e.cfg.Server.DevExtraHosts {
		extraHosts = append(extraHosts, h+":host-gateway")
	}

	resp, err := e.docker.ContainerCreate(ctx, &container.Config{
		Image:      addl.image,
		Cmd:        []string{"cat"},
		OpenStdin:  true, // so cat stays alive :3
		Tty:        false,
		Hostname:   "spindle",
		WorkingDir: workspaceDir,
		Labels: map[string]string{
			"sh.tangled.pipeline/workflow_id": wid.String(),
		},
	}, &container.HostConfig{
		Mounts: append([]mount.Mount{
			{
				Type:     mount.TypeTmpfs,
				Target:   "/tmp",
				ReadOnly: false,
				TmpfsOptions: &mount.TmpfsOptions{
					Mode: 0o1777, // world-writable sticky bit
					Options: [][]string{
						{"exec"},
					},
				},
			},
		}, addl.mounts...),
		ReadonlyRootfs: false,
		CapDrop:        []string{"ALL"},
		CapAdd:         []string{"CAP_DAC_OVERRIDE", "CAP_CHOWN", "CAP_FOWNER", "CAP_SETUID", "CAP_SETGID"},
		SecurityOpt:    []string{"no-new-privileges"},
		ExtraHosts:     extraHosts,
		Resources: container.Resources{
			Memory: e.cfg.DaggerPipelines.MaxJobMemoryMB * 1024 * 1024,
		},
	}, nil, nil, "")
	if err != nil {
		fmt.Fprintf(
			wfLogger.DataWriter(setupStepIdx, "stderr"),
			"container creation failed: %s",
			err,
		)
		return fmt.Errorf("creating container: %w", err)
	}

	e.registerCleanup(wid, func(ctx context.Context) error {
		if err := e.docker.ContainerStop(ctx, resp.ID, container.StopOptions{}); err != nil {
			return fmt.Errorf("stopping container: %w", err)
		}

		err := e.docker.ContainerRemove(ctx, resp.ID, container.RemoveOptions{
			RemoveVolumes: true,
			RemoveLinks:   false,
			Force:         false,
		})
		if err != nil {
			return fmt.Errorf("removing container: %w", err)
		}

		return nil
	})

	wfLogger.DataWriter(setupStepIdx, "stdout").Write([]byte("Starting container..."))
	if err := e.docker.ContainerStart(ctx, resp.ID, container.StartOptions{}); err != nil {
		return fmt.Errorf("starting container: %w", err)
	}

	mkExecResp, err := e.docker.ContainerExecCreate(ctx, resp.ID, container.ExecOptions{
		Cmd:          []string{"mkdir", "-p", workspaceDir, homeDir, shimDir},
		AttachStdout: true,
		AttachStderr: true,
	})
	if err != nil {
		return err
	}

	execResp, err := e.docker.ContainerExecAttach(ctx, mkExecResp.ID, container.ExecAttachOptions{})
	if err != nil {
		return err
	}
	defer execResp.Close()

	// waiting on the output is how we wait on the command
	if _, err := io.ReadAll(execResp.Reader); err != nil {
		return err
	}

	execInspectResp, err := e.docker.ContainerExecInspect(ctx, mkExecResp.ID)
	if err != nil {
		return err
	}

	if execInspectResp.ExitCode != 0 {
		return fmt.Errorf("mkdir exited with exit code %d", execInspectResp.ExitCode)
	} else if execInspectResp.Running {
		return errors.New("mkdir is somehow still running??")
	}

	addl.container = resp.ID
	wf.Data = addl

	return nil
}

func (e *Engine) RunStep(ctx context.Context, wid models.WorkflowId, w *models.Workflow, idx int, secrets []secrets.UnlockedSecret, wfLogger models.WorkflowLogger) error {
	addl := w.Data.(addlFields)

	workflowEnvs := ConstructEnvs(w.Environment)
	for _, s := range secrets {
		workflowEnvs.AddEnv(s.Key, s.Value)
	}

	step := w.Steps[idx]

	select {
	case <-ctx.Done():
		return ctx.Err()
	default:
	}

	envs := append(EnvVars(nil), workflowEnvs...)
	if dstep, ok := step.(Step); ok {
		for k, v := range dstep.environment {
			envs.AddEnv(k, v)
		}
	}
	envs = append(envs, e.daggerEnvs(addl)...)

	mkExecResp, err := e.docker.ContainerExecCreate(ctx, addl.container, container.ExecOptions{
		Cmd:          []string{"bash", "-c", step.Command()},
		AttachStdout: true,
		AttachStderr: true,
		Env:          envs,
	})
	if err != nil {
		return fmt.Errorf("User step error:\ncreating exec: %w", err)
	}

	tailDone := make(chan error, 1)
	go func() {
		tailDone <- e.tailStep(ctx, wfLogger, mkExecResp.ID, idx)
	}()

	select {
	case <-tailDone:

	case <-ctx.Done():
		e.l.Warn("step timed out", "step", step.Name())

		<-tailDone

		return engine.ErrTimedOut
	}

	select {
	case <-ctx.Done():
		return ctx.Err()
	default:
	}

	execInspectResp, err := e.docker.ContainerExecInspect(ctx, mkExecResp.ID)
	if err != nil {
		return fmt.Errorf("User step error:\n%w", err)
	}

	if execInspectResp.ExitCode != 0 {
		inspectResp, err := e.docker.ContainerInspect(ctx, addl.container)
		if err != nil {
			return fmt.Errorf("User step error:\n%w", err)
		}

		e.l.Error("workflow failed!", "workflow_id", wid.String(), "exit_code", execInspectResp.ExitCode, "oom_killed", inspectResp.State.OOMKilled)

		if inspectResp.State.OOMKilled {
			return fmt.Errorf("User step error:\n%w", ErrOOMKilled)
		}
		return fmt.Errorf("User step error: exited with code %d", execInspectResp.ExitCode)
	}

	return nil
}

func (e *Engine) daggerEnvs(addl addlFields) EnvVars {
	var envs EnvVars

	envs.AddEnv("HOME", homeDir)
	existingPath := "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
	envs.AddEnv("PATH", fmt.Sprintf(
		"%s:%s:%s/.nix-profile/bin:/nix/var/nix/profiles/default/bin:%s",
		shimDir, cliDir, homeDir, existingPath,
	))

	envs.AddEnv("NO_COLOR", "1")
	envs.AddEnv("DAGGER_NO_NAG", "1")

	if addl.module != "" {
		envs.AddEnv(moduleEnv, addl.module)
	}
	if addl.version != "" {
		envs.AddEnv(versionEnv, addl.version)
	}
	if host := e.cfg.DaggerPipelines.RunnerHost; host != "" {
		envs.AddEnv("_EXPERIMENTAL_DAGGER_RUNNER_HOST", host)
	}
	if token := e.cfg.DaggerPipelines.CloudToken; token != "" {
		envs.AddEnv("DAGGER_CLOUD_TOKEN", token)
	}
	if sock := e.cfg.Server.DockerSocket; sock != "" {
		envs.AddEnv("DOCKER_HOST", fmt.Sprintf("unix://%s", sock))
	}

	return envs
}

func (e *Engine) tailStep(ctx context.Context, wfLogger models.WorkflowLogger, execID string, stepIdx int) error {
	if wfLogger == nil {
		return nil
	}

	// This actually *starts* the command. Thanks, Docker!
	logs, err := e.docker.ContainerExecAttach(ctx, execID, container.ExecAttachOptions{})
	if err != nil {
		return err
	}
	defer logs.Close()

	_, err = stdcopy.StdCopy(
		wfLogger.DataWriter(stepIdx, "stdout"),
		wfLogger.DataWriter(stepIdx, "stderr"),
		logs.Reader,
	)
	if err != nil && err != io.EOF && !errors.Is(err, context.DeadlineExceeded) {
		return fmt.Errorf("failed to copy logs: %w", err)
	}

	return nil
}

func (e *Engine) DestroyWorkflow(ctx context.Context, wid models.WorkflowId) error {
	fns := e.drainCleanups(wid)

	for _, fn := range fns {
		if err := fn(ctx); err != nil {
			e.l.Error("failed to cleanup workflow resource", "workflowId", wid, "error", err)
		}
	}
	return nil
}

func (e *Engine) registerCleanup(wid models.WorkflowId, fn cleanupFunc) {
	e.cleanupMu.Lock()
	defer e.cleanupMu.Unlock()

	key := wid.String()
	e.cleanup[key] = append(e.cleanup[key], fn)
}

func (e *Engine) drainCleanups(wid models.WorkflowId) []cleanupFunc {
	e.cleanupMu.Lock()
	key := wid.String()

	fns := e.cleanup[key]
	delete(e.cleanup, key)
	e.cleanupMu.Unlock()

	return fns
}

func networkName(wid models.WorkflowId) string {
	return fmt.Sprintf("dagger-workflow-network-%s", wid)
}
