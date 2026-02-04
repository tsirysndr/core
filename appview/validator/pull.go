package validator

import (
	"database/sql"
	"fmt"
	"strings"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
	"tangled.org/core/patchutil"
)

func (v *Validator) ValidatePull(pull *models.Pull) error {
	if len(pull.Submissions) == 0 {
		return fmt.Errorf("pull must have at least one submission")
	}

	latestSubmission := pull.LatestSubmission()
	if latestSubmission == nil {
		return fmt.Errorf("pull must have a valid latest submission")
	}

	isFormatPatch := patchutil.IsFormatPatch(latestSubmission.Patch)

	// title and body can only be empty if the patch is a format-patch
	if !isFormatPatch {
		if pull.Title == "" {
			return fmt.Errorf("pull title is empty (required for non-format-patch pulls)")
		}

		if pull.Body == "" {
			return fmt.Errorf("pull body is empty (required for non-format-patch pulls)")
		}

		if st := strings.TrimSpace(v.sanitizer.SanitizeDescription(pull.Title)); st == "" {
			return fmt.Errorf("title is empty after HTML sanitization")
		}

		if sb := strings.TrimSpace(v.sanitizer.SanitizeDefault(pull.Body)); sb == "" {
			return fmt.Errorf("body is empty after HTML sanitization")
		}
	}

	// the dependent_on should not form a DAG, aka, two PRs should not have the same dependent
	if pull.DependentOn != nil {
		dependentPull, err := db.GetPull(
			v.db,
			orm.FilterEq("dependent_on", pull.DependentOn.String()),
		)

		if err == sql.ErrNoRows {
			return nil
		}

		if err != nil {
			return fmt.Errorf("failed to fetch pulls with same dependency: %w", err)
		}

		if dependentPull.AtUri() == pull.AtUri() {
			return nil
		}

		return fmt.Errorf("another pull already depends on %s, which would form a DAG, this is presently disallowed", pull.DependentOn.String())
	}

	return nil
}
