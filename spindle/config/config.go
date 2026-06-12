package config

import (
	"context"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sethvargo/go-envconfig"
	"tangled.org/core/xrpc/serviceauth"
)

type Server struct {
	ListenAddr        string   `env:"LISTEN_ADDR, default=0.0.0.0:6555"`
	DBPath            string   `env:"DB_PATH, default=spindle.db"`
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

type S3 struct {
	LogBucket string `env:"LOG_BUCKET"`
}

type MicroVMPipelines struct {
	ImageDir        string `env:"IMAGE_DIR, required"`
	OverlayDir      string `env:"OVERLAY_DIR, default="` // where microVM temporary disks will live
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

	EnableCgroups    bool   `env:"ENABLE_CGROUPS, default=false"`
	CgroupParent     string `env:"CGROUP_PARENT, default=self"`
	CgroupPidsMax    int64  `env:"CGROUP_PIDS_MAX, default=4096"`
	CgroupSwapMaxMiB *int64 `env:"CGROUP_SWAP_MAX_MIB"`
	// memory.min that will get assigned to the supervisor (spindle itself) cgroup
	CgroupSupervisorMemoryMinMiB int64 `env:"CGROUP_SUPERVISOR_MEMORY_MIN_MIB, default=512"`
}

type NixCache struct {
	ReadURLs          []string `env:"READ_URLS"`
	TrustedPublicKeys []string `env:"TRUSTED_PUBLIC_KEYS"`
	UploadURL         string   `env:"UPLOAD_URL"`
}

type Config struct {
	Server           Server           `env:",prefix=SPINDLE_SERVER_"`
	NixeryPipelines  NixeryPipelines  `env:",prefix=SPINDLE_NIXERY_PIPELINES_"`
	MicroVMPipelines MicroVMPipelines `env:",prefix=SPINDLE_MICROVM_PIPELINES_"`
	NixCache         NixCache         `env:",prefix=SPINDLE_NIX_CACHE_"`
	S3               S3               `env:",prefix=SPINDLE_S3_"`
}

func Load(ctx context.Context) (*Config, error) {
	var cfg Config
	err := envconfig.Process(ctx, &cfg)
	if err != nil {
		return nil, err
	}

	return &cfg, nil
}
