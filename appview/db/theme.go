package db

import (
	"database/sql"

	"tangled.org/core/appview/models"
)

func GetThemePreference(e Execer, did string) (models.ThemePreference, error) {
	var theme string
	err := e.QueryRow(
		`select theme from theme_preferences where user_did = ?`,
		did,
	).Scan(&theme)
	if err == sql.ErrNoRows {
		return models.ThemePreference(models.ThemeAuto), nil
	}
	if err != nil {
		return "", err
	}
	return models.ThemePreference(theme), nil
}

func UpsertThemePreference(e Execer, did string, theme models.ThemePreference) error {
	_, err := e.Exec(
		`insert into theme_preferences (user_did, theme, updated_at) values (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         on conflict(user_did) do update set theme = excluded.theme, updated_at = excluded.updated_at`,
		did,
		string(theme),
	)
	return err
}
