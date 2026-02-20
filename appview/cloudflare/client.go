package cloudflare

import (
	"fmt"

	cf "github.com/cloudflare/cloudflare-go/v6"
	"github.com/cloudflare/cloudflare-go/v6/option"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	"tangled.org/core/appview/config"
)

// Client holds all cloudflare service clients used by the appview.
type Client struct {
	api    *cf.Client // scoped to DNS / zone operations
	kvAPI  *cf.Client // scoped to Workers KV (separate token)
	s3     *s3.Client
	zone   string
	kvNS   string
	bucket string
	cfAcct string
}

func New(c *config.Config) (*Client, error) {
	api := cf.NewClient(option.WithAPIToken(c.Cloudflare.ApiToken))

	kvAPI := cf.NewClient(option.WithAPIToken(c.Cloudflare.KV.ApiToken))

	// R2 endpoint is S3-compatible, keyed by account id.
	r2Endpoint := fmt.Sprintf("https://%s.r2.cloudflarestorage.com", c.Cloudflare.AccountId)

	s3Client := s3.New(s3.Options{
		BaseEndpoint: aws.String(r2Endpoint),
		Region:       "auto",
		Credentials: credentials.NewStaticCredentialsProvider(
			c.Cloudflare.R2.AccessKeyID,
			c.Cloudflare.R2.SecretAccessKey,
			"",
		),
		UsePathStyle: true,
	})

	return &Client{
		api:    api,
		kvAPI:  kvAPI,
		s3:     s3Client,
		zone:   c.Cloudflare.ZoneId,
		kvNS:   c.Cloudflare.KV.NamespaceId,
		bucket: c.Cloudflare.R2.Bucket,
		cfAcct: c.Cloudflare.AccountId,
	}, nil
}

// Enabled returns true when the client has enough config to perform site
// operations. Callers should check this before attempting any CF operations
// so the appview degrades gracefully in dev environments without credentials.
func (cl *Client) Enabled() bool {
	return cl != nil &&
		cl.cfAcct != "" &&
		cl.kvNS != "" &&
		cl.bucket != ""
}
