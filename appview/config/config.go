package config

import (
	"context"
	"fmt"
	"net"
	"net/url"
	"strings"
	"time"

	"github.com/sethvargo/go-envconfig"

	"tangled.org/core/consts"
)

type CoreConfig struct {
	CookieSecret            string `env:"COOKIE_SECRET, default=00000000000000000000000000000000"`
	DbPath                  string `env:"DB_PATH, default=appview.db"`
	ListenAddr              string `env:"LISTEN_ADDR, default=0.0.0.0:3000"`
	MetricsListenAddr       string `env:"METRICS_LISTEN_ADDR, default=0.0.0.0:9090"`
	AppviewHost             string `env:"APPVIEW_HOST, default=tangled.org"`
	AppviewName             string `env:"APPVIEW_NAME, default=Tangled"`
	Dev                     bool   `env:"DEV, default=false"`
	DisallowedNicknamesFile string `env:"DISALLOWED_NICKNAMES_FILE"`

	// temporarily, to add users to default knot and spindle
	AppPassword string `env:"APP_PASSWORD"`

	// uhhhh this is because knot1 is under icy's did
	TmpAltAppPassword string `env:"ALT_APP_PASSWORD"`
}

func (c *CoreConfig) UseTLS() bool {
	return !c.Dev
}

func (c *CoreConfig) BaseUrl() string {
	if c.UseTLS() {
		return "https://" + c.AppviewHost
	}
	return "http://" + c.AppviewHost
}

// Hostname returns AppviewHost with any port stripped, for consumers that need
// a bare host (e.g. an ssh destination) rather than a URL authority.
func (c *CoreConfig) Hostname() string {
	if host, _, err := net.SplitHostPort(c.AppviewHost); err == nil {
		return host
	}
	return c.AppviewHost
}

type OAuthConfig struct {
	ClientSecret string `env:"CLIENT_SECRET"`
	ClientKid    string `env:"CLIENT_KID"`
}

type PlcConfig struct {
	PLCURL string `env:"URL, default=https://plc.directory"`
}

type KnotMirrorConfig struct {
	Url                  string        `env:"URL, default=https://mirror.tangled.network"`
	ArchiveHeaderTimeout time.Duration `env:"ARCHIVE_HEADER_TIMEOUT, default=60s"`
}

type JetstreamConfig struct {
	Endpoint string `env:"ENDPOINT, default=wss://jetstream1.us-east.bsky.network/subscribe"`
}

type ConsumerConfig struct {
	RetryInterval     time.Duration `env:"RETRY_INTERVAL, default=60s"`
	MaxRetryInterval  time.Duration `env:"MAX_RETRY_INTERVAL, default=120m"`
	ConnectionTimeout time.Duration `env:"CONNECTION_TIMEOUT, default=5s"`
	WorkerCount       int           `env:"WORKER_COUNT, default=64"`
	QueueSize         int           `env:"QUEUE_SIZE, default=100"`
}

type ResendConfig struct {
	ApiKey              string `env:"API_KEY"`
	SentFrom            string `env:"SENT_FROM, default=noreply@notifs.tangled.sh"`
	NewsletterSegmentId string `env:"NEWSLETTER_SEGMENT_ID"`
	AssetsURL           string `env:"ASSETS_URL, default=https://assets.tangled.network/email/"`
}

type CamoConfig struct {
	Host         string `env:"HOST, default=https://camo.tangled.sh"`
	SharedSecret string `env:"SHARED_SECRET"`
}

func (c *CamoConfig) Enabled() bool {
	return c.SharedSecret != ""
}

type AvatarConfig struct {
	Host         string `env:"HOST, default=https://avatar.tangled.sh"`
	SharedSecret string `env:"SHARED_SECRET"`
}

type PosthogConfig struct {
	ApiKey   string `env:"API_KEY"`
	Endpoint string `env:"ENDPOINT, default=https://eu.i.posthog.com"`
}

type RedisConfig struct {
	Addr     string `env:"ADDR, default=localhost:6379"`
	Password string `env:"PASS"`
	DB       int    `env:"DB, default=0"`
}

type PdsConfig struct {
	Host        string `env:"HOST, default=https://tngl.sh"`
	UserDomain  string `env:"USER_DOMAIN, default=.tngl.sh"`
	AdminSecret string `env:"ADMIN_SECRET"`
}

func (p *PdsConfig) IsTnglShUser(pdsHost string) bool {
	return strings.TrimRight(pdsHost, "/") == strings.TrimRight(p.Host, "/")
}

type KnotConfig struct {
	Default     string `env:"DEFAULT"`
	AdminSecret string `env:"ADMIN_SECRET"`
}

type R2Config struct {
	AccessKeyID     string `env:"ACCESS_KEY_ID"`
	SecretAccessKey string `env:"SECRET_ACCESS_KEY"`
	Bucket          string `env:"BUCKET, default=tangled-sites"`
}

type TurnstileConfig struct {
	SiteKey   string `env:"SITE_KEY"`
	SecretKey string `env:"SECRET_KEY"`
}

type KVConfig struct {
	NamespaceId string `env:"NAMESPACE_ID"`
	ApiToken    string `env:"API_TOKEN"`
}

type Cloudflare struct {
	// Legacy top-level API token. For services like Workers KV, we
	// now use a scoped Account API token configured under the relevant
	// sub-struct.
	ApiToken  string `env:"API_TOKEN"`
	ZoneId    string `env:"ZONE_ID"`
	AccountId string `env:"ACCOUNT_ID"`

	KV        KVConfig        `env:",prefix=KV_"`
	Turnstile TurnstileConfig `env:",prefix=TURNSTILE_"`
	R2        R2Config        `env:",prefix=R2_"`
}

type SitesConfig struct {
	Domain string `env:"DOMAIN, default=tngl.io"`
}

type LabelConfig struct {
	DefaultLabelDefs []string `env:"DEFAULTS, default=at://did:plc:wshs7t2adsemcrrd4snkeqli/sh.tangled.label.definition/wontfix,at://did:plc:wshs7t2adsemcrrd4snkeqli/sh.tangled.label.definition/good-first-issue,at://did:plc:wshs7t2adsemcrrd4snkeqli/sh.tangled.label.definition/duplicate,at://did:plc:wshs7t2adsemcrrd4snkeqli/sh.tangled.label.definition/documentation,at://did:plc:wshs7t2adsemcrrd4snkeqli/sh.tangled.label.definition/assignee"` // delimiter=,
	GoodFirstIssue   string   `env:"GFI, default=at://did:plc:wshs7t2adsemcrrd4snkeqli/sh.tangled.label.definition/good-first-issue"`
}

type BlueskyConfig struct {
	UpdateInterval time.Duration `env:"UPDATE_INTERVAL, default=1h"`
}

type OgreConfig struct {
	Host string `env:"HOST, default=https://ogre.tangled.network"`
}

type CodeSearchConfig struct {
	ZoektUrl string `env:"ZOEKT_URL"`
}

type SSHConfig struct {
	Enabled     bool   `env:"ENABLED, default=false"`
	ListenAddr  string `env:"LISTEN_ADDR, default=0.0.0.0:3333"`
	HostKeyPath string `env:"HOST_KEY_PATH"`
}

func (cfg RedisConfig) ToURL() string {
	u := &url.URL{
		Scheme: "redis",
		Host:   cfg.Addr,
		Path:   fmt.Sprintf("/%d", cfg.DB),
	}

	if cfg.Password != "" {
		u.User = url.UserPassword("", cfg.Password)
	}

	return u.String()
}

type Config struct {
	Core          CoreConfig       `env:",prefix=TANGLED_"`
	Jetstream     JetstreamConfig  `env:",prefix=TANGLED_JETSTREAM_"`
	Knotstream    ConsumerConfig   `env:",prefix=TANGLED_KNOTSTREAM_"`
	Spindlestream ConsumerConfig   `env:",prefix=TANGLED_SPINDLESTREAM_"`
	Resend        ResendConfig     `env:",prefix=TANGLED_RESEND_"`
	Posthog       PosthogConfig    `env:",prefix=TANGLED_POSTHOG_"`
	Camo          CamoConfig       `env:",prefix=TANGLED_CAMO_"`
	Avatar        AvatarConfig     `env:",prefix=TANGLED_AVATAR_"`
	OAuth         OAuthConfig      `env:",prefix=TANGLED_OAUTH_"`
	Redis         RedisConfig      `env:",prefix=TANGLED_REDIS_"`
	Plc           PlcConfig        `env:",prefix=TANGLED_PLC_"`
	Pds           PdsConfig        `env:",prefix=TANGLED_PDS_"`
	Knot          KnotConfig       `env:",prefix=TANGLED_KNOT_"`
	Cloudflare    Cloudflare       `env:",prefix=TANGLED_CLOUDFLARE_"`
	Label         LabelConfig      `env:",prefix=TANGLED_LABEL_"`
	Bluesky       BlueskyConfig    `env:",prefix=TANGLED_BLUESKY_"`
	Sites         SitesConfig      `env:",prefix=TANGLED_SITES_"`
	KnotMirror    KnotMirrorConfig `env:",prefix=TANGLED_KNOTMIRROR_"`
	Ogre          OgreConfig       `env:",prefix=TANGLED_OGRE_"`
	SSH           SSHConfig        `env:",prefix=TANGLED_SSH_"`
	CodeSearch    CodeSearchConfig `env:",prefix=TANGLED_CODESEARCH_"`
}

func LoadConfig(ctx context.Context) (*Config, error) {
	var cfg Config
	err := envconfig.Process(ctx, &cfg)
	if err != nil {
		return nil, err
	}

	if cfg.Knot.Default == "" {
		cfg.Knot.Default = consts.DefaultKnot
	}

	return &cfg, nil
}
