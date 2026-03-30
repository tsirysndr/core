package commitverify

import (
	"log"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/crypto"
	"tangled.org/core/types"
)

type verifiedCommit struct {
	fingerprint string
	hash        string
}

type VerifiedCommits map[verifiedCommit]struct{}

func (vcs VerifiedCommits) IsVerified(hash string) bool {
	for vc := range vcs {
		if vc.hash == hash {
			return true
		}
	}
	return false
}

func (vcs VerifiedCommits) Fingerprint(hash string) string {
	for vc := range vcs {
		if vc.hash == hash {
			return vc.fingerprint
		}
	}
	return ""
}

func GetVerifiedCommits(e db.Execer, emailToDid map[string]string, ndCommits []types.Commit) (VerifiedCommits, error) {
	vcs := VerifiedCommits{}

	didPubkeyCache := make(map[string][]models.PublicKey)

	for _, commit := range ndCommits {
		// skip unsigned commits early: no signature means no DB lookup needed,
		// and most commits in a typical log are unsigned.
		if commit.PGPSignature == "" {
			continue
		}

		committerEmail := commit.Committer.Email
		if did, exists := emailToDid[committerEmail]; exists {
			// check if we've already fetched public keys for this did
			pubKeys, ok := didPubkeyCache[did]
			if !ok {
				// fetch and cache public keys
				keys, err := db.GetPublicKeysForDid(e, did)
				if err != nil {
					log.Printf("failed to fetch pubkey for %s: %v", committerEmail, err)
					continue
				}
				pubKeys = keys
				didPubkeyCache[did] = pubKeys
			}

			// try to verify with any associated pubkeys
			payload := commit.Payload()
			signature := commit.PGPSignature
			for _, pk := range pubKeys {
				if _, ok := crypto.VerifySignature([]byte(pk.Key), []byte(signature), []byte(payload)); ok {

					fp, err := crypto.SSHFingerprint(pk.Key)
					if err != nil {
						log.Println("error computing ssh fingerprint:", err)
					}

					vc := verifiedCommit{fingerprint: fp, hash: commit.This}
					vcs[vc] = struct{}{}
					break
				}
			}

		}
	}

	return vcs, nil
}
