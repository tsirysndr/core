package state

import (
	"net/http"
	"strconv"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
)

func onboardingStepURL(step int) string {
	switch step {
	case models.OnboardingStepProfile:
		return "/welcome/profile"
	case models.OnboardingStepKeys:
		return "/welcome/keys"
	case models.OnboardingStepSocial:
		return "/welcome/social"
	case models.OnboardingStepRepo:
		return "/welcome/repo"
	default:
		return "/"
	}
}

func (s *State) OnboardingResume(w http.ResponseWriter, r *http.Request) {
	did := s.oauth.GetDid(r)
	ob, err := db.GetOnboarding(s.db, did)
	if err != nil {
		s.logger.Error("failed to get onboarding", "did", did, "err", err)
	}
	if ob == nil || ob.Status != models.OnboardingInProgress {
		http.Redirect(w, r, "/", http.StatusFound)
		return
	}
	http.Redirect(w, r, onboardingStepURL(ob.Step), http.StatusFound)
}

func (s *State) OnboardingNext(w http.ResponseWriter, r *http.Request) {
	did := s.oauth.GetDid(r)
	step, _ := strconv.Atoi(r.FormValue("step"))

	if step >= models.OnboardingStepDone {
		if err := db.CompleteOnboarding(s.db, did); err != nil {
			s.logger.Error("failed to complete onboarding", "did", did, "err", err)
		}
		s.pages.HxRedirect(w, "/")
		return
	}

	if step == models.OnboardingStepRepo {
		if err := db.CompleteOnboarding(s.db, did); err != nil {
			s.logger.Error("failed to complete onboarding", "did", did, "err", err)
		}
		s.pages.HxRedirect(w, onboardingStepURL(step))
		return
	}

	if err := db.AdvanceOnboardingStep(s.db, did, step); err != nil {
		s.logger.Error("failed to advance onboarding", "did", did, "err", err)
	}
	s.pages.HxRedirect(w, onboardingStepURL(step))
}

func (s *State) OnboardingSkip(w http.ResponseWriter, r *http.Request) {
	did := s.oauth.GetDid(r)
	if err := db.SkipOnboarding(s.db, did); err != nil {
		s.logger.Error("failed to skip onboarding", "did", did, "err", err)
	}
	s.pages.HxRedirect(w, "/")
}

func (s *State) OnboardingComplete(w http.ResponseWriter, r *http.Request) {
	did := s.oauth.GetDid(r)
	if err := db.CompleteOnboarding(s.db, did); err != nil {
		s.logger.Error("failed to complete onboarding", "did", did, "err", err)
	}
	s.pages.HxRedirect(w, "/")
}

func (s *State) OnboardingProfile(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)

	profile, err := db.GetProfile(s.db, user.Did)
	if err != nil {
		s.logger.Error("getting profile data", "did", user.Did, "err", err)
	}
	if profile == nil {
		profile = &models.Profile{Did: user.Did}
	}

	var alsoKnownAs []string
	if ident, err := s.idResolver.ResolveIdent(r.Context(), user.Did); err == nil {
		alsoKnownAs = ident.AlsoKnownAs
	}

	bp := pages.BaseParamsFromContext(r.Context())
	s.pages.Onboarding(w, pages.OnboardingParams{
		BaseParams: bp,
		Step:       models.OnboardingStepProfile,
		EditBio: pages.EditBioParams{
			BaseParams:  bp,
			Profile:     profile,
			AlsoKnownAs: alsoKnownAs,
			Action:      "/welcome/profile",
		},
	})
}

func (s *State) OnboardingSaveProfile(w http.ResponseWriter, r *http.Request) {
	did := s.oauth.GetDid(r)

	if err := r.ParseForm(); err != nil {
		s.pages.Notice(w, "update-profile", "Invalid form.")
		return
	}

	profile, err := s.bioFormToProfile(r)
	if err != nil {
		s.pages.Notice(w, "update-profile", err.Error())
		return
	}

	if err := s.writeProfile(r, profile); err != nil {
		s.logger.Error("onboarding: failed to write profile", "did", did, "err", err)
		s.pages.Notice(w, "update-profile", "Failed to update profile, try again later.")
		return
	}

	if err := db.AdvanceOnboardingStep(s.db, did, models.OnboardingStepSocial); err != nil {
		s.logger.Error("failed to advance onboarding", "did", did, "err", err)
	}
	s.pages.HxRedirect(w, "/welcome/social")
}

func (s *State) OnboardingKeys(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)

	pubKeys, err := db.GetPublicKeysForDid(s.db, user.Did)
	if err != nil {
		s.logger.Error("getting public keys", "did", user.Did, "err", err)
	}

	s.pages.Onboarding(w, pages.OnboardingParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		Step:       models.OnboardingStepKeys,
		PubKeys:    pubKeys,
	})
}

const onboardingSocialLimit = 4

func (s *State) OnboardingSocial(w http.ResponseWriter, r *http.Request) {
	user := s.oauth.GetMultiAccountUser(r)
	bp := pages.BaseParamsFromContext(r.Context())

	var people []pages.FollowCard
	mostFollowed, err := db.GetMostFollowed(s.db, onboardingSocialLimit+2)
	if err != nil {
		s.logger.Error("failed to get most followed", "err", err)
	}
	var dids []string
	for _, did := range mostFollowed {
		if did == user.Did {
			continue
		}
		dids = append(dids, did)
		if len(dids) >= onboardingSocialLimit {
			break
		}
	}
	if len(dids) > 0 {
		statuses, _ := db.GetFollowStatuses(s.db, user.Did, dids)
		counts, _ := db.GetFollowerFollowingCounts(s.db, dids)
		for _, did := range dids {
			profile, _ := db.GetProfile(s.db, did)
			if profile == nil {
				profile = &models.Profile{Did: did}
			}
			people = append(people, pages.FollowCard{
				BaseParams:     bp,
				UserDid:        did,
				Profile:        profile,
				FollowStatus:   statuses[did],
				FollowersCount: counts[did].Followers,
				FollowingCount: counts[did].Following,
			})
		}
	}

	repos, err := db.GetTopStarredReposLastWeek(s.db)
	if err != nil {
		s.logger.Error("failed to get trending repos", "err", err)
	}
	trending := make([]models.Repo, 0, onboardingSocialLimit)
	for _, repo := range repos {
		if repo.RepoDid == "" {
			continue
		}
		trending = append(trending, repo)
		if len(trending) >= onboardingSocialLimit {
			break
		}
	}

	starStatuses := map[string]bool{}
	if len(trending) > 0 {
		repoDids := make([]string, 0, len(trending))
		for _, repo := range trending {
			repoDids = append(repoDids, repo.RepoDid)
		}
		starStatuses, _ = db.GetStarStatuses(s.db, user.Did, repoDids)
	}

	s.pages.Onboarding(w, pages.OnboardingParams{
		BaseParams:    bp,
		Step:          models.OnboardingStepSocial,
		People:        people,
		TrendingRepos: trending,
		StarStatuses:  starStatuses,
	})
}

func (s *State) OnboardingRepo(w http.ResponseWriter, r *http.Request) {
	s.pages.Onboarding(w, pages.OnboardingParams{
		BaseParams: pages.BaseParamsFromContext(r.Context()),
		Step:       models.OnboardingStepRepo,
	})
}
