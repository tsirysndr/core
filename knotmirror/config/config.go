package config

import (
	"context"
	"time"

	"github.com/sethvargo/go-envconfig"
)

type Config struct {
	PlcUrl              string        `env:"MIRROR_PLC_URL, default=https://plc.directory"`
	TapUrl              string        `env:"MIRROR_TAP_URL, default=http://localhost:2480"`
	DbUrl               string        `env:"MIRROR_DB_URL, required"`
	KnotUseSSL          bool          `env:"MIRROR_KNOT_USE_SSL, default=false"` // use SSL for Knot when not scheme is not specified
	KnotSSRF            bool          `env:"MIRROR_KNOT_SSRF, default=false"`
	GitRepoBasePath     string        `env:"MIRROR_GIT_BASEPATH, default=repos"`
	GitRepoFetchTimeout time.Duration `env:"MIRROR_GIT_FETCH_TIMEOUT, default=600s"`
	ResyncParallelism   int           `env:"MIRROR_RESYNC_PARALLELISM, default=5"`
	Slurper             SlurperConfig `env:",prefix=MIRROR_SLURPER_"`
	UseSSL              bool          `env:"MIRROR_USE_SSL, default=false"`
	Hostname            string        `env:"MIRROR_HOSTNAME, required"`
	Listen              string        `env:"MIRROR_LISTEN, default=:7000"`
	MetricsListen       string        `env:"MIRROR_METRICS_LISTEN, default=127.0.0.1:7100"`
	AdminListen         string        `env:"MIRROR_ADMIN_LISTEN, default=127.0.0.1:7200"`
}

func (c *Config) BaseUrl() string {
	if c.UseSSL {
		return "https://" + c.Hostname
	}
	return "http://" + c.Hostname
}

type SlurperConfig struct {
	PersistCursorPeriod time.Duration `env:"PERSIST_CURSOR_PERIOD, default=4s"`
	ConcurrencyPerHost  int           `env:"CONCURRENCY, default=4"`
}

func Load(ctx context.Context) (*Config, error) {
	var cfg Config
	if err := envconfig.Process(ctx, &cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}
