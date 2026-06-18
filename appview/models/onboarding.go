package models

import (
	"time"
)

type OnboardingStatus string

const (
	OnboardingInProgress OnboardingStatus = "in_progress"
	OnboardingCompleted  OnboardingStatus = "completed"
	OnboardingSkipped    OnboardingStatus = "skipped"
)

// onboarding steps, in order. Done is a sentinel that marks completion.
const (
	OnboardingStepProfile = 0
	OnboardingStepSocial  = 1
	OnboardingStepKeys    = 2
	OnboardingStepRepo    = 3
	OnboardingStepDone    = 4
)

type Onboarding struct {
	Did     string
	Step    int
	Status  OnboardingStatus
	Created time.Time
	Updated time.Time
}

// OnboardingProgress feeds the "resume onboarding" banner/panel. Active is false
// when there is nothing to resume (no in-progress onboarding).
type OnboardingProgress struct {
	Active  bool
	Step    int
	Total   int
	Percent int
}

// Progress derives display progress for the resume banner/panel. It is nil-safe,
// so callers can pass the result of GetOnboarding directly.
func (o *Onboarding) Progress() OnboardingProgress {
	if o == nil || o.Status != OnboardingInProgress {
		return OnboardingProgress{}
	}
	// Percent reflects completed steps out of the total, so the bar only fills
	// as steps are finished (e.g. on the last step it is not yet 100%).
	total := OnboardingStepDone
	percent := o.Step * 100 / total
	if percent > 100 {
		percent = 100
	}
	return OnboardingProgress{
		Active:  true,
		Step:    o.Step,
		Total:   total,
		Percent: percent,
	}
}
