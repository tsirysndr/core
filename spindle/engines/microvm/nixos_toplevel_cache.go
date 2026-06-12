package microvm

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"tangled.org/core/spindle/db"
)

const nixosToplevelCacheSchemaVersion = 1

type nixosToplevelCacheRecord struct {
	ConfigKey string    `json:"config_key"`
	Toplevel  string    `json:"toplevel"`
	UpdatedAt time.Time `json:"updated_at"`
}

type nixosToplevelCacheStore struct {
	db *db.DB
}

func newNixOSToplevelCacheStore(d *db.DB) nixosToplevelCacheStore {
	return nixosToplevelCacheStore{db: d}
}

func (s nixosToplevelCacheStore) Lookup(configKey string) (nixosToplevelCacheRecord, bool, error) {
	if s.db == nil {
		return nixosToplevelCacheRecord{}, false, nil
	}
	r, err := s.db.GetNixOSToplevelCacheRecord(configKey)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nixosToplevelCacheRecord{}, false, nil
		}
		return nixosToplevelCacheRecord{}, false, err
	}
	return nixosToplevelCacheRecord{
		ConfigKey: r.ConfigKey,
		Toplevel:  r.Toplevel,
		UpdatedAt: r.UpdatedAt,
	}, true, nil
}

func (s nixosToplevelCacheStore) Commit(configKey, toplevel string) error {
	if configKey == "" {
		return fmt.Errorf("config key is empty")
	}
	if toplevel == "" {
		return fmt.Errorf("config toplevel is empty")
	}
	if s.db == nil {
		return nil
	}
	return s.db.SaveNixOSToplevelCacheRecord(configKey, toplevel)
}

func BaseConfigHash(imageSpec ImageSpec) (string, error) {
	if imageSpec.BaseConfigHash == "" {
		return "", fmt.Errorf("microvm image spec missing baseConfigHash")
	}
	return imageSpec.BaseConfigHash, nil
}

func userConfigHash(cfg manifestConfig) string {
	data, _ := json.Marshal(cfg)
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

func buildConfigKey(imageSpec ImageSpec, cfg manifestConfig) (string, error) {
	baseHash, err := BaseConfigHash(imageSpec)
	if err != nil {
		return "", err
	}
	payload := struct {
		Schema     int    `json:"schema"`
		BaseConfig string `json:"base_config"`
		UserConfig string `json:"user_config"`
	}{
		Schema:     nixosToplevelCacheSchemaVersion,
		BaseConfig: baseHash,
		UserConfig: userConfigHash(cfg),
	}
	data, _ := json.Marshal(payload)
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:]), nil
}

func BuildConfigKey(imageSpec ImageSpec, userConfigJSON string) (string, error) {
	var cfg manifestConfig
	if err := json.Unmarshal([]byte(userConfigJSON), &cfg); err != nil {
		return "", err
	}
	return buildConfigKey(imageSpec, cfg)
}
