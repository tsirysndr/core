package db

import (
	"database/sql"
	"log"
	"time"

	"tangled.org/core/appview/models"
)

// GetOnboarding returns the onboarding record for a did, or (nil, nil) if none exists.
func GetOnboarding(e Execer, did string) (*models.Onboarding, error) {
	query := `select did, step, status, created, updated from onboarding where did = ?`
	row := e.QueryRow(query, did)

	var o models.Onboarding
	var status string
	var created, updated string
	err := row.Scan(&o.Did, &o.Step, &status, &created, &updated)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}

	o.Status = models.OnboardingStatus(status)
	if t, err := time.Parse(time.RFC3339, created); err == nil {
		o.Created = t
	}
	if t, err := time.Parse(time.RFC3339, updated); err == nil {
		o.Updated = t
	}

	return &o, nil
}

func UpsertOnboarding(e Execer, o *models.Onboarding) error {
	query := `
		insert into onboarding (did, step, status, updated)
		values (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
		on conflict(did) do update set
			step = excluded.step,
			status = excluded.status,
			updated = excluded.updated
	`
	_, err := e.Exec(query, o.Did, o.Step, string(o.Status))
	return err
}

// AdvanceOnboardingStep moves the stored step forward to the given step. It is
// monotonic: it never decreases the step, so navigating back to an earlier page
// and re-submitting does not regress recorded progress.
func AdvanceOnboardingStep(e Execer, did string, step int) error {
	_, err := e.Exec(
		`update onboarding set step = ?, updated = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') where did = ? and step < ?`,
		step, did, step,
	)
	return err
}

func CompleteOnboarding(e Execer, did string) error {
	_, err := e.Exec(
		`update onboarding set status = ?, step = ?, updated = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') where did = ?`,
		string(models.OnboardingCompleted), models.OnboardingStepDone, did,
	)
	return err
}

func SkipOnboarding(e Execer, did string) error {
	_, err := e.Exec(
		`update onboarding set status = ?, updated = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') where did = ?`,
		string(models.OnboardingSkipped), did,
	)
	return err
}

// IsOnboarding reports whether a did has an in-progress onboarding record.
func IsOnboarding(e Execer, did string) (bool, error) {
	var exists bool
	err := e.QueryRow(
		`select exists(select 1 from onboarding where did = ? and status = 'in_progress')`,
		did,
	).Scan(&exists)
	if err != nil {
		log.Println("failed to check onboarding status", err)
		return false, err
	}
	return exists, nil
}
