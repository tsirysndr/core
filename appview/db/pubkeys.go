package db

import (
	"time"

	"tangled.org/core/appview/models"
)

func UpsertPublicKey(e Execer, pubKey models.PublicKey) error {
	_, err := e.Exec(
		`insert into public_keys (did, rkey, name, key, created)
		values (?, ?, ?, ?, ?)
		on conflict(did, rkey) do update set
			name    = excluded.name,
			key     = excluded.key,
			created = excluded.created`,
		pubKey.Did,
		pubKey.Rkey,
		pubKey.Name,
		pubKey.Key,
		pubKey.Created.Format(time.RFC3339),
	)
	return err
}

func UpdatePublicKey(e Execer, did, name, key, rkey string) error {
	_, err := e.Exec(
		`update public_keys set name = ? where did = ? and key = ? and rkey = ?`,
		name, did, key, rkey)
	return err
}

// for public_keys with empty rkey
func DeletePublicKeyLegacy(e Execer, did, name string) error {
	_, err := e.Exec(`
		delete from public_keys
		where did = ? and name = ? and rkey = ''`,
		did, name)
	return err
}

func DeletePublicKeyByRkey(e Execer, did, rkey string) error {
	_, err := e.Exec(`
		delete from public_keys
		where did = ? and rkey = ?`,
		did, rkey)
	return err
}

func GetAllPublicKeys(e Execer) ([]models.PublicKey, error) {
	var keys []models.PublicKey

	rows, err := e.Query(`select key, name, did, rkey, created from public_keys`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var publicKey models.PublicKey
		var createdAt string
		if err := rows.Scan(&publicKey.Key, &publicKey.Name, &publicKey.Did, &publicKey.Rkey, &createdAt); err != nil {
			return nil, err
		}
		createdAtTime, _ := time.Parse(time.RFC3339, createdAt)
		publicKey.Created = &createdAtTime
		keys = append(keys, publicKey)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return keys, nil
}

func GetPublicKeysForDid(e Execer, did string) ([]models.PublicKey, error) {
	var keys []models.PublicKey

	rows, err := e.Query(`select did, key, name, rkey, created from public_keys where did = ?`, did)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var publicKey models.PublicKey
		var createdAt string
		if err := rows.Scan(&publicKey.Did, &publicKey.Key, &publicKey.Name, &publicKey.Rkey, &createdAt); err != nil {
			return nil, err
		}
		createdAtTime, _ := time.Parse(time.RFC3339, createdAt)
		publicKey.Created = &createdAtTime
		keys = append(keys, publicKey)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return keys, nil
}
