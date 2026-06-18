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
	total := OnboardingStepDone
	percent := min(o.Step*100/total, 100)
	return OnboardingProgress{
		Active:  true,
		Step:    o.Step,
		Total:   total,
		Percent: percent,
	}
}
