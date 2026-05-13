package config

import (
	"context"
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sethvargo/go-envconfig"
)

type Server struct {
	ListenAddr             string  `env:"LISTEN_ADDR, default=0.0.0.0:6555"`
	DBPath                 string  `env:"DB_PATH, default=spindle.db"`
	Hostname               string  `env:"HOSTNAME, required"`
	JetstreamEndpoint      string  `env:"JETSTREAM_ENDPOINT, default=wss://jetstream1.us-west.bsky.network/subscribe"`
	Tap                    Tap     `env:",prefix=TAP_"`
	PlcUrl                 string  `env:"PLC_URL, default=https://plc.directory"`
	Dev                    bool    `env:"DEV, default=false"`
	Owner                  string  `env:"OWNER, required"`
	Secrets                Secrets `env:",prefix=SECRETS_"`
	LogDir                 string  `env:"LOG_DIR, default=/var/log/spindle"`
	QueueSize              int     `env:"QUEUE_SIZE, default=100"`
	MaxJobCount            int     `env:"MAX_JOB_COUNT, default=2"`            // max number of pipelines that run at a time
	MaxConcurrentWorkflows int     `env:"MAX_CONCURRENT_WORKFLOWS, default=8"` // max number of workflow containers running at once (memory cap)
	DockerSocket           string  `env:"DOCKER_SOCKET"`                        // path to a docker socket to expose to workflow containers
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
	return syntax.DID(fmt.Sprintf("did:web:%s", s.Hostname))
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
	Nixery          string `env:"NIXERY, default=nixery.tangled.sh"`
	WorkflowTimeout string `env:"WORKFLOW_TIMEOUT, default=5m"`
	MaxJobMemoryMB  int64  `env:"MAX_JOB_MEMORY_MB, default=6144"` // per-container memory limit in MiB (default 6 GiB)
}

type S3 struct {
	LogBucket string `env:"LOG_BUCKET"`
}

type Config struct {
	Server          Server          `env:",prefix=SPINDLE_SERVER_"`
	NixeryPipelines NixeryPipelines `env:",prefix=SPINDLE_NIXERY_PIPELINES_"`
	S3              S3              `env:",prefix=SPINDLE_S3_"`
}

func Load(ctx context.Context) (*Config, error) {
	var cfg Config
	err := envconfig.Process(ctx, &cfg)
	if err != nil {
		return nil, err
	}

	return &cfg, nil
}
