package models

import (
	"encoding/json"
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/gliderlabs/ssh"
	"tangled.org/core/api/tangled"
)

type PublicKey struct {
	Did     string `json:"did"`
	Key     string `json:"key"`
	Name    string `json:"name"`
	Rkey    string `json:"rkey"`
	Created *time.Time
}

func (p PublicKey) MarshalJSON() ([]byte, error) {
	type Alias PublicKey
	return json.Marshal(&struct {
		Created string `json:"created"`
		*Alias
	}{
		Created: p.Created.Format(time.RFC3339),
		Alias:   (*Alias)(&p),
	})
}

func (p *PublicKey) AsRecord() tangled.PublicKey {
	return tangled.PublicKey{
		Name:      p.Name,
		Key:       p.Key,
		CreatedAt: p.Created.Format(time.RFC3339),
	}
}

var _ Validator = new(PublicKey)

func (p *PublicKey) Validate() error {
	if _, _, _, _, err := ssh.ParseAuthorizedKey([]byte(p.Key)); err != nil {
		return fmt.Errorf("invalid ssh key format: %w", err)
	}

	return nil
}

func PublicKeyFromRecord(did syntax.DID, rkey syntax.RecordKey, record tangled.PublicKey) (PublicKey, error) {
	created, err := time.Parse(time.RFC3339, record.CreatedAt)
	if err != nil {
		return PublicKey{}, fmt.Errorf("invalid time format '%s'", record.CreatedAt)
	}

	return PublicKey{
		Did:     did.String(),
		Rkey:    rkey.String(),
		Name:    record.Name,
		Key:     record.Key,
		Created: &created,
	}, nil
}
