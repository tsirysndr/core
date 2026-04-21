package pulls

import (
	"log"
	"net/http"
	"time"

	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/ogre"
	"tangled.org/core/patchutil"
)

func (s *Pulls) PullOpenGraphSummary(w http.ResponseWriter, r *http.Request) {
	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		log.Println("failed to get repo and knot", err)
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		log.Println("pull not found in context")
		http.Error(w, "pull not found", http.StatusNotFound)
		return
	}

	ownerHandle := s.pages.DisplayHandle(r.Context(), f.Did)
	authorHandle := s.pages.DisplayHandle(r.Context(), pull.OwnerDid)

	avatarUrl := s.pages.AvatarUrl(f.Did, "256")
	authorAvatarUrl := s.pages.AvatarUrl(pull.OwnerDid, "256")

	var status string
	if pull.State.IsOpen() {
		status = "open"
	} else if pull.State.IsMerged() {
		status = "merged"
	} else {
		status = "closed"
	}

	var filesChanged int
	var additions int64
	var deletions int64

	if len(pull.Submissions) > 0 {
		latestSubmission := pull.LatestSubmission()
		niceDiff := patchutil.AsNiceDiff(latestSubmission.Patch, pull.TargetBranch)
		filesChanged = niceDiff.Stat.FilesChanged
		additions = int64(niceDiff.Stat.Insertions)
		deletions = int64(niceDiff.Stat.Deletions)
	}

	commentCount := pull.TotalComments()

	reactionCount, _ := db.GetReactionCount(s.db, pull.AtUri())

	rounds := max(1, len(pull.Submissions))

	payload := ogre.PullRequestCardPayload{
		Type:              "pullRequest",
		RepoName:          f.Name,
		OwnerHandle:       ownerHandle,
		AuthorHandle:      authorHandle,
		AvatarUrl:         avatarUrl,
		AuthorAvatarUrl:   authorAvatarUrl,
		Title:             pull.Title,
		PullRequestNumber: pull.PullId,
		Status:            status,
		FilesChanged:      filesChanged,
		Additions:         int(additions),
		Deletions:         int(deletions),
		Rounds:            rounds,
		CommentCount:      commentCount,
		ReactionCount:     reactionCount,
		CreatedAt:         pull.Created.Format(time.RFC3339),
	}

	imageBytes, err := s.ogreClient.RenderPullRequestCard(r.Context(), payload)
	if err != nil {
		log.Println("failed to render pull request card", err)
		http.Error(w, "failed to render pull request card", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "image/png")
	w.Header().Set("Cache-Control", "public, max-age=3600")
	w.WriteHeader(http.StatusOK)
	_, err = w.Write(imageBytes)
	if err != nil {
		log.Println("failed to write pull request card", err)
		return
	}
}
