package pulls

import (
	"net/http"
	"strconv"

	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"

	"github.com/go-chi/chi/v5"
)

func (s *Pulls) PullComment(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "PullComment")

	user := s.oauth.GetMultiAccountUser(r)
	if user != nil {
		l = l.With("user", user.Did)
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		l.Error("failed to get pull")
		s.pages.Notice(w, "pull-error", "Failed to edit patch. Try again later.")
		return
	}
	l = l.With("pull_id", pull.PullId, "pull_owner", pull.OwnerDid)

	roundNumberStr := chi.URLParam(r, "round")
	roundNumber, err := strconv.Atoi(roundNumberStr)
	if err != nil || roundNumber >= len(pull.Submissions) {
		http.Error(w, "bad round id", http.StatusBadRequest)
		l.Error("failed to parse round id", "err", err, "round_number_str", roundNumberStr)
		return
	}

	switch r.Method {
	case http.MethodGet:
		s.pages.PullNewCommentFragment(w, pages.PullNewCommentParams{
			LoggedInUser: user,
			RepoInfo:     s.repoResolver.GetRepoInfo(r, user),
			Pull:         pull,
			RoundNumber:  roundNumber,
		})
		return
	}
}
