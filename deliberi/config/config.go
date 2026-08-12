package config

import (
	"context"

	"github.com/sethvargo/go-envconfig"
)

type Config struct {
	// ListenAddr is where deliberi's xrpc + health server binds.
	ListenAddr string `env:"DELIBERI_LISTEN_ADDR, default=0.0.0.0:6565"`

	// Hostname is deliberi's public hostname; it derives deliberi's did:web,
	// used as the audience for verifying inbound service-auth tokens.
	Hostname string `env:"DELIBERI_HOSTNAME, required"`

	DbPath string `env:"DELIBERI_DB_PATH, default=deliberi.db"`

	PlcUrl            string `env:"DELIBERI_PLC_URL, default=https://plc.directory"`
	JetstreamEndpoint string `env:"DELIBERI_JETSTREAM_ENDPOINT, default=wss://jetstream1.us-east.bsky.network/subscribe"`

	// BobbinApiUrl hosts listRecipients (subscriber fan-out); called as a
	// plain internal xrpc, no auth.
	BobbinApiUrl string `env:"DELIBERI_BOBBIN_API_URL, default=https://api.tangled.org"`

	// BaseURL is the public frontend URL used to build links in digest emails.
	BaseURL string `env:"DELIBERI_BASE_URL, default=https://tangled.org"`

	Pds    PdsConfig    `env:",prefix=DELIBERI_PDS_"`
	Resend ResendConfig `env:",prefix=DELIBERI_RESEND_"`

	Dev bool `env:"DELIBERI_DEV, default=false"`
}

type PdsConfig struct {
	Host        string `env:"HOST, default=https://tngl.sh"`
	UserDomain  string `env:"USER_DOMAIN, default=.tngl.sh"`
	AdminSecret string `env:"ADMIN_SECRET"`
}

type ResendConfig struct {
	ApiKey    string `env:"API_KEY"`
	SentFrom  string `env:"SENT_FROM, default=noreply@notifs.tangled.sh"`
	AssetsURL string `env:"ASSETS_URL, default=https://assets.tangled.network/email/"`
}

func (c *Config) SignupEnabled() bool {
	return c.Pds.AdminSecret != ""
}

func Load(ctx context.Context) (*Config, error) {
	var cfg Config
	if err := envconfig.Process(ctx, &cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}
