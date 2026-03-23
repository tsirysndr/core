package issues

import (
	"context"
	"log"
	"net/http"
	"time"

	"tangled.org/core/appview/models"
	"tangled.org/core/ogre"
)

func (rp *Issues) IssueOpenGraphSummary(w http.ResponseWriter, r *http.Request) {
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		log.Println("failed to get repo and knot", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		log.Println("issue not found in context")
		http.Error(w, "issue not found", http.StatusNotFound)
		return
	}

	var ownerHandle string
	owner, err := rp.idResolver.ResolveIdent(context.Background(), f.Did)
	if err != nil {
		ownerHandle = f.Did
	} else {
		ownerHandle = "@" + owner.Handle.String()
	}

	var authorHandle string
	author, err := rp.idResolver.ResolveIdent(context.Background(), issue.Did)
	if err != nil {
		authorHandle = issue.Did
	} else {
		authorHandle = "@" + author.Handle.String()
	}

	avatarUrl := rp.pages.AvatarUrl(authorHandle, "256")

	status := "closed"
	if issue.Open {
		status = "open"
	}

	commentCount := len(issue.Comments)

	payload := ogre.IssueCardPayload{
		Type:          "issue",
		RepoName:      f.Name,
		OwnerHandle:   ownerHandle,
		AvatarUrl:     avatarUrl,
		Title:         issue.Title,
		IssueNumber:   issue.IssueId,
		Status:        status,
		Labels:        []ogre.LabelData{},
		CommentCount:  commentCount,
		ReactionCount: 0,
		CreatedAt:     issue.Created.Format(time.RFC3339),
	}

	imageBytes, err := rp.ogreClient.RenderIssueCard(r.Context(), payload)
	if err != nil {
		log.Println("failed to render issue card", err)
		http.Error(w, "failed to render issue card", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "image/png")
	w.Header().Set("Cache-Control", "public, max-age=3600")
	w.WriteHeader(http.StatusOK)
	_, err = w.Write(imageBytes)
	if err != nil {
		log.Println("failed to write issue card", err)
		return
	}
}
