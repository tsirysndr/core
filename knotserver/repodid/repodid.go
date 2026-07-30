package repodid

import (
	"context"
	"fmt"
	"net/url"
	"strings"

	atcrypto "github.com/bluesky-social/indigo/atproto/atcrypto"
	"github.com/did-method-plc/go-didplc/didplc"
	"tangled.org/core/idresolver"
	"tangled.org/core/repoident"
)

type PreparedDID struct {
	RepoDid       string
	SigningKeyRaw []byte
	op            *didplc.RegularOp
	plcUrl        string
}

func PrepareRepoDID(plcUrl, knotServiceUrl string) (*PreparedDID, error) {
	if plcUrl == "" {
		return nil, fmt.Errorf("PLC directory URL is not configured")
	}
	parsed, parseErr := url.Parse(plcUrl)
	if parseErr != nil || parsed.Host == "" || (parsed.Scheme != "http" && parsed.Scheme != "https") {
		return nil, fmt.Errorf("PLC directory URL is invalid: %q", plcUrl)
	}

	privKey, err := atcrypto.GeneratePrivateKeyK256()
	if err != nil {
		return nil, fmt.Errorf("generating signing key: %w", err)
	}

	pubKey, err := privKey.PublicKey()
	if err != nil {
		return nil, fmt.Errorf("deriving public key: %w", err)
	}

	didKey := pubKey.DIDKey()

	op := didplc.RegularOp{
		Type:         "plc_operation",
		RotationKeys: []string{didKey},
		VerificationMethods: map[string]string{
			"atproto": didKey,
		},
		AlsoKnownAs: []string{},
		Services: map[string]didplc.OpService{
			repoident.LegacyKnotServiceID: {
				Type:     repoident.LegacyKnotServiceType,
				Endpoint: knotServiceUrl,
			},
		},
		Prev: nil,
	}

	if err := op.Sign(privKey); err != nil {
		return nil, fmt.Errorf("signing genesis op: %w", err)
	}

	repoDid, err := op.DID()
	if err != nil {
		return nil, fmt.Errorf("deriving DID from genesis: %w", err)
	}

	return &PreparedDID{
		RepoDid:       repoDid,
		SigningKeyRaw: privKey.Bytes(),
		op:            &op,
		plcUrl:        plcUrl,
	}, nil
}

func (p *PreparedDID) Submit(ctx context.Context) error {
	plcClient := didplc.Client{
		DirectoryURL: p.plcUrl,
		UserAgent:    "tangled-knot",
	}
	if err := plcClient.Submit(ctx, p.RepoDid, p.op); err != nil {
		return fmt.Errorf("submitting to PLC directory: %w", err)
	}
	return nil
}

const maxDidWebLength = 256

func VerifyRepoDIDWeb(ctx context.Context, resolver *idresolver.Resolver, repoDid, knotServiceUrl string) error {
	if !strings.HasPrefix(repoDid, "did:web:") {
		return fmt.Errorf("expected did:web, got: %s", repoDid)
	}

	if len(repoDid) > maxDidWebLength {
		return fmt.Errorf("did:web exceeds maximum length of %d characters", maxDidWebLength)
	}

	authority := strings.TrimPrefix(repoDid, "did:web:")
	if colonIdx := strings.IndexByte(authority, ':'); colonIdx >= 0 {
		authority = authority[:colonIdx]
	}
	if authority == "" || strings.ContainsAny(authority, "/#?@ ") {
		return fmt.Errorf("did:web has invalid authority: %s", repoDid)
	}

	ident, err := resolver.ResolveIdent(ctx, repoDid)
	if err != nil {
		return fmt.Errorf("resolving did:web document: %w", err)
	}

	knotEndpoint, err := repoident.KnotURLFromIdentity(ident, repoident.AllowHTTP)
	if err != nil {
		return fmt.Errorf("did:web document: %w", err)
	}
	expected, err := repoident.ParseKnotURL(knotServiceUrl, repoident.AllowHTTP)
	if err != nil {
		return fmt.Errorf("knot service URL %q: %w", knotServiceUrl, err)
	}
	if knotEndpoint != expected {
		return fmt.Errorf(
			"did:web knot service endpoint %q doesn't match this knot %q",
			knotEndpoint, expected,
		)
	}

	if _, err := ident.PublicKey(); err != nil {
		return fmt.Errorf("did:web document missing valid atproto verification method: %w", err)
	}

	return nil
}
