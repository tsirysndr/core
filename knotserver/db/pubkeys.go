package db

import (
	"database/sql"
	"log/slog"
	"strconv"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"golang.org/x/crypto/ssh"
	"tangled.org/core/api/tangled"
)

type PublicKey struct {
	Did  syntax.DID
	Rkey syntax.RecordKey
	tangled.PublicKey
}

func (d *DB) UpsertPublicKey(pk PublicKey) error {
	tx, err := d.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	if pk.Rkey != "" {
		if _, err := tx.Exec(`delete from public_keys where did = ? and rkey = ?`, pk.Did, pk.Rkey); err != nil {
			return err
		}
	}

	if err := insertPublicKey(tx, d.logger, pk); err != nil {
		return err
	}

	return tx.Commit()
}

func insertPublicKey(tx *sql.Tx, logger *slog.Logger, pk PublicKey) error {
	if pk.Key == "" {
		logger.Warn("skipping public key with empty key value", "did", pk.Did, "rkey", pk.Rkey)
		return nil
	}

	canonical, ok := normalizePublicKey(pk.Key)
	if !ok {
		logger.Warn("skipping malformed public key", "did", pk.Did, "rkey", pk.Rkey)
		return nil
	}
	pk.Key = canonical

	if pk.CreatedAt == "" {
		pk.CreatedAt = time.Now().Format(time.RFC3339)
	}

	res, err := tx.Exec(
		`insert or ignore into public_keys (did, key, rkey, created) values (?, ?, ?, ?)`,
		pk.Did, pk.Key, pk.Rkey, pk.CreatedAt,
	)
	if err != nil {
		return err
	}

	if rows, err := res.RowsAffected(); err == nil && rows == 0 {
		logger.Warn("public key not stored, already registered to another did", "did", pk.Did, "rkey", pk.Rkey)
	}

	return nil
}

func (d *DB) DeletePublicKeyByRkey(did syntax.DID, rkey syntax.RecordKey) error {
	if rkey == "" {
		return nil
	}

	query := `delete from public_keys where did = ? and rkey = ?`
	_, err := d.db.Exec(query, did, rkey)
	return err
}

func (d *DB) ReplacePublicKeys(did syntax.DID, keys []PublicKey) error {
	tx, err := d.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	if _, err := tx.Exec(`delete from public_keys where did = ?`, did); err != nil {
		return err
	}

	if err := insertPublicKeys(tx, d.logger, keys); err != nil {
		return err
	}

	return tx.Commit()
}

func insertPublicKeys(tx *sql.Tx, logger *slog.Logger, keys []PublicKey) error {
	if len(keys) == 0 {
		return nil
	}

	if err := insertPublicKey(tx, logger, keys[0]); err != nil {
		return err
	}

	return insertPublicKeys(tx, logger, keys[1:])
}

func (pk *PublicKey) JSON() map[string]any {
	return map[string]any{
		"did":       pk.Did,
		"key":       pk.Key,
		"createdAt": pk.CreatedAt,
	}
}

func normalizePublicKey(key string) (string, bool) {
	parsed, comment, _, _, err := ssh.ParseAuthorizedKey([]byte(key))
	if err != nil {
		return "", false
	}

	canonical := strings.TrimSpace(string(ssh.MarshalAuthorizedKey(parsed)))
	if comment != "" {
		canonical += " " + comment
	}

	return canonical, true
}

func (d *DB) DidForPublicKey(offered ssh.PublicKey) (syntax.DID, bool, error) {
	prefix := strings.TrimSpace(string(ssh.MarshalAuthorizedKey(offered)))

	var did syntax.DID
	err := d.db.QueryRow(
		`select did from public_keys where key = ? or key like ? limit 1`,
		prefix, prefix+" %",
	).Scan(&did)
	if err == sql.ErrNoRows {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}

	return did, true, nil
}

func (d *DB) GetAllPublicKeys() ([]PublicKey, error) {
	var keys []PublicKey

	rows, err := d.db.Query(`select key, did, created from public_keys`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var publicKey PublicKey
		if err := rows.Scan(&publicKey.Key, &publicKey.Did, &publicKey.CreatedAt); err != nil {
			return nil, err
		}
		keys = append(keys, publicKey)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	return keys, nil
}

func (d *DB) GetPublicKeysPaginated(limit int, cursor string) ([]PublicKey, string, error) {
	var keys []PublicKey

	offset := 0
	if cursor != "" {
		if o, err := strconv.Atoi(cursor); err == nil && o >= 0 {
			offset = o
		}
	}

	query := `select key, did, created from public_keys order by created desc limit ? offset ?`
	rows, err := d.db.Query(query, limit+1, offset) // +1 to check if there are more results
	if err != nil {
		return nil, "", err
	}
	defer rows.Close()

	for rows.Next() {
		var publicKey PublicKey
		if err := rows.Scan(&publicKey.Key, &publicKey.Did, &publicKey.CreatedAt); err != nil {
			return nil, "", err
		}
		keys = append(keys, publicKey)
	}

	if err := rows.Err(); err != nil {
		return nil, "", err
	}

	// check if there are more results for pagination
	var nextCursor string
	if len(keys) > limit {
		keys = keys[:limit] // remove the extra item
		nextCursor = strconv.Itoa(offset + limit)
	}

	return keys, nextCursor, nil
}
