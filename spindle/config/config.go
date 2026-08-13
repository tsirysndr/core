package config

import (
	"context"
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sethvargo/go-envconfig"
	"tangled.org/core/xrpc/serviceauth"
)

type Server struct {
	ListenAddr        string   `env:"LISTEN_ADDR, default=0.0.0.0:6555"`
	DBPath            string   `env:"DB_PATH, default=spindle.db"`
	RepoDir           string   `env:"REPO_DIR, default=repos"`
	Hostname          string   `env:"HOSTNAME, required"`
	JetstreamEndpoint string   `env:"JETSTREAM_ENDPOINT, default=wss://jetstream1.us-west.bsky.network/subscribe"`
	Tap               Tap      `env:",prefix=TAP_"`
	PlcUrl            string   `env:"PLC_URL, default=https://plc.directory"`
	Dev               bool     `env:"DEV, default=false"`
	DevExtraHosts     []string `env:"DEV_EXTRA_HOSTS"`
	Owner             string   `env:"OWNER, required"`
	Secrets           Secrets  `env:",prefix=SECRETS_"`
	LogDir            string   `env:"LOG_DIR, default=/var/log/spindle"`
	QueueSize         int      `env:"QUEUE_SIZE, default=100"`
	MaxJobCount       int      `env:"MAX_JOB_COUNT, default=2"` // max number of pipelines that run at a time
	DockerSocket      string   `env:"DOCKER_SOCKET"`            // path to a docker socket to expose to workflow containers
}

type Tap struct {
	Embed         bool   `env:"EMBED, default=true"`
	Url           string `env:"URL, default=http://[::1]:2480"`
	Bind          string `env:"BIND, default=[::1]:2480"`
	DBPath        string `env:"DB_PATH, default=tap.db"`
	RelayUrl      string `env:"RELAY_URL, default=https://bsky.network"`
	AdminPassword string `env:"ADMIN_PASSWORD"`
}

func (s Server) Did() syntax.DID {
	return serviceauth.DidWeb(s.Hostname)
}

type Secrets struct {
	Provider string        `env:"PROVIDER, default=sqlite"`
	OpenBao  OpenBaoConfig `env:",prefix=OPENBAO_"`
}

type OpenBaoConfig struct {
	ProxyAddr string `env:"PROXY_ADDR, default=http://127.0.0.1:8200"`
	Mount     string `env:"MOUNT, default=spindle"`
}

type NixeryPipelines struct {
	Nixery                 string `env:"NIXERY, default=nixery.tangled.sh"`
	WorkflowTimeout        string `env:"WORKFLOW_TIMEOUT, default=5m"`
	MaxJobMemoryMB         int64  `env:"MAX_JOB_MEMORY_MB, default=6144"`     // per-container memory limit in MiB (default 6 GiB)
	MaxConcurrentWorkflows int    `env:"MAX_CONCURRENT_WORKFLOWS, default=8"` // max number of workflow containers running at once (memory cap)
}

type ArtifactStoreDisk struct {
	Dir string `env:"DIR"`
}

type ArtifactStoreS3 struct {
	Bucket string `env:"BUCKET"`
	Region string `env:"REGION, default=us-east-1"`
}

type ArtifactStores struct {
	Disk ArtifactStoreDisk `env:",prefix=DISK_"`
	S3   ArtifactStoreS3   `env:",prefix=S3_"`
}

type LegacyS3 struct {
	LogBucket string `env:"LOG_BUCKET"`
}

type MicroVMPipelines struct {
	ImageDir        string `env:"IMAGE_DIR"`
	OverlayDir      string `env:"OVERLAY_DIR"` // where microVM temporary disks will live
	DefaultImage    string `env:"DEFAULT_IMAGE, default=nixos-x86_64"`
	AgentPort       uint32 `env:"AGENT_PORT, default=10240"`
	EnableKVM       bool   `env:"ENABLE_KVM, default=true"`
	WorkflowTimeout string `env:"WORKFLOW_TIMEOUT, default=5m"`

	MaxTotalMemoryMiB int64 `env:"MAX_TOTAL_MEMORY_MIB, default=0"`
	MaxTotalVCPUs     int64 `env:"MAX_TOTAL_VCPUS, default=0"`
	MaxTotalDiskMiB   int64 `env:"MAX_TOTAL_DISK_MIB, default=0"`

	MaxWorkflowMemoryMiB int64 `env:"MAX_WORKFLOW_MEMORY_MIB, default=0"`
	MaxWorkflowVCPUs     int64 `env:"MAX_WORKFLOW_VCPUS, default=0"`
	MaxWorkflowDiskMiB   int64 `env:"MAX_WORKFLOW_DISK_MIB, default=0"`

	AgingThreshold time.Duration `env:"AGING_THRESHOLD, default=30s"`

	DebugSSH DebugSSH `env:",prefix=DEBUG_SSH_"`

	EnableCgroups    bool   `env:"ENABLE_CGROUPS, default=false"`
	CgroupParent     string `env:"CGROUP_PARENT, default=self"`
	CgroupPidsMax    int64  `env:"CGROUP_PIDS_MAX, default=4096"`
	CgroupSwapMaxMiB *int64 `env:"CGROUP_SWAP_MAX_MIB"`
	// cpu.max quota as a percentage of one core. 0 caps each vm at its
	// configured vcpu count, negative disables the limit
	CgroupCPUMaxPercent int64 `env:"CGROUP_CPU_MAX_PERCENT, default=0"`
	// io.weight for workflow cgroups (1-10000, kernel default 100). 0 leaves
	// io unlimited
	CgroupIOWeight uint64 `env:"CGROUP_IO_WEIGHT, default=0"`
	// memory.min that will get assigned to the supervisor (spindle itself) cgroup
	CgroupSupervisorMemoryMinMiB int64 `env:"CGROUP_SUPERVISOR_MEMORY_MIN_MIB, default=512"`
}

type DebugSSH struct {
	Enabled    bool   `env:"ENABLED, default=false"`
	ListenAddr string `env:"LISTEN_ADDR, default=0.0.0.0:2222"`
	JumpHost   string `env:"JUMP_HOST"`
	Host       string `env:"HOST"`
	// path to private key; if empty, spindle will generate one next to the db
	HostKeyPath string `env:"HOST_KEY_PATH"`
	// how long to keep a failed wf alive after failure, for sshing in
	GracePeriod time.Duration `env:"GRACE_PERIOD, default=5m"`
}

type NixCache struct {
	ReadURLs          []string `env:"READ_URLS"`
	TrustedPublicKeys []string `env:"TRUSTED_PUBLIC_KEYS"`
	UploadURL         string   `env:"UPLOAD_URL"`
}

// governs how spindle places and runs jobs
type Role string

const (
	RoleStandalone Role = "standalone"
	RoleMill       Role = "mill"
	RoleExecutor   Role = "executor"
)

// fields are selectively active depending on the role
type Mill struct {
	URL                string        `env:"URL"`                          // mill websocket endpoint dialled by the executor
	SharedSecret       string        `env:"SHARED_SECRET"`                // the executor's token for dialing the mill
	MaxPending         int           `env:"MAX_PENDING, default=100"`     // mill pending job queue limit
	ReconnectGrace     time.Duration `env:"RECONNECT_GRACE, default=45s"` // reconnect window before leases are failed
	Seats              int           `env:"SEATS, default=4"`             // executor seats advertised to the mill
	Labels             []string      `env:"LABELS"`                       // executor capability labels
	ArtifactStore      string        `env:"ARTIFACT_STORE"`               // store shared by mill and its executors
	JumpListenAddr     string        `env:"JUMP_LISTEN_ADDR"`
	JumpHostKeyPath    string        `env:"JUMP_HOST_KEY_PATH"`
	DebugExecutorPort  uint32        `env:"DEBUG_EXECUTOR_PORT, default=2223"`
	MaxJumpConnections int           `env:"MAX_JUMP_CONNECTIONS, default=128"`
}

type Config struct {
	Role             Role             `env:"SPINDLE_ROLE, default=standalone"`
	Server           Server           `env:",prefix=SPINDLE_SERVER_"`
	NixeryPipelines  NixeryPipelines  `env:",prefix=SPINDLE_NIXERY_PIPELINES_"`
	MicroVMPipelines MicroVMPipelines `env:",prefix=SPINDLE_MICROVM_PIPELINES_"`
	NixCache         NixCache         `env:",prefix=SPINDLE_NIX_CACHE_"`
	ArtifactStores   ArtifactStores   `env:",prefix=SPINDLE_ARTIFACT_STORES_"`
	LegacyS3         LegacyS3         `env:",prefix=SPINDLE_S3_"`
	Mill             Mill             `env:",prefix=SPINDLE_MILL_"`
}

func (c *Config) validate() error {
	switch c.Role {
	case RoleStandalone, RoleMill:
		if c.Mill.URL != "" {
			return fmt.Errorf("SPINDLE_MILL_URL is set but SPINDLE_ROLE=%s; only an executor dials a mill", c.Role)
		}
	case RoleExecutor:
		if c.Mill.URL == "" {
			return fmt.Errorf("SPINDLE_ROLE=executor requires SPINDLE_MILL_URL (the mill to dial)")
		}
		if c.Mill.SharedSecret == "" {
			return fmt.Errorf("SPINDLE_ROLE=executor requires SPINDLE_MILL_SHARED_SECRET (its executor token)")
		}
	default:
		return fmt.Errorf("unknown SPINDLE_ROLE %q (want standalone, mill, or executor)", c.Role)
	}
	if c.Mill.JumpListenAddr != "" {
		if c.Role != RoleMill {
			return fmt.Errorf("SPINDLE_MILL_JUMP_LISTEN_ADDR requires SPINDLE_ROLE=mill")
		}
		if c.Mill.JumpHostKeyPath == "" {
			return fmt.Errorf("SPINDLE_MILL_JUMP_LISTEN_ADDR requires SPINDLE_MILL_JUMP_HOST_KEY_PATH")
		}
		if c.Mill.MaxJumpConnections <= 0 {
			return fmt.Errorf("SPINDLE_MILL_MAX_JUMP_CONNECTIONS must be greater than zero")
		}
	}
	return nil
}

func Load(ctx context.Context) (*Config, error) {
	var cfg Config
	err := envconfig.Process(ctx, &cfg)
	if err != nil {
		return nil, err
	}
	if err := cfg.validate(); err != nil {
		return nil, err
	}

	return &cfg, nil
}
