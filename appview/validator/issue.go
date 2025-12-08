package validator

import (
	"fmt"
	"strings"

	"tangled.org/core/appview/models"
)

func (v *Validator) ValidateIssue(issue *models.Issue) error {
	if issue.Title == "" {
		return fmt.Errorf("issue title is empty")
	}

	if issue.Body == "" {
		return fmt.Errorf("issue body is empty")
	}

	if st := strings.TrimSpace(v.sanitizer.SanitizeDescription(issue.Title)); st == "" {
		return fmt.Errorf("title is empty after HTML sanitization")
	}

	if sb := strings.TrimSpace(v.sanitizer.SanitizeDefault(issue.Body)); sb == "" {
		return fmt.Errorf("body is empty after HTML sanitization")
	}

	return nil
}
