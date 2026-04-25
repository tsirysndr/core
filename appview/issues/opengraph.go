package issues

import (
	"context"
	"fmt"
	"net/http"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/ogre"
	"tangled.org/core/orm"
)

func (rp *Issues) IssueOpenGraphSummary(w http.ResponseWriter, r *http.Request) {
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		rp.logger.Error("failed to get repo and knot", "err", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		rp.logger.Error("issue not found in context")
		http.Error(w, "issue not found", http.StatusNotFound)
		return
	}

	labelDefs, err := db.GetLabelDefinitions(
		rp.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", tangled.RepoIssueNSID),
	)
	if err != nil {
		rp.logger.Error("failed to fetch label definitions", "err", err)
		http.Error(w, "label definitions not found", http.StatusInternalServerError)
		return
	}

	defs := make(map[string]*models.LabelDefinition)
	for _, l := range labelDefs {
		defs[l.AtUri().String()] = &l
	}

	labels := []ogre.LabelData{}
	for _, def := range defs {
		for val := range issue.Labels.GetValSet(def.AtUri().String()) {
			name := def.Name
			value := ""

			if !def.ValueType.IsNull() {
				name = fmt.Sprintf("%s/", def.Name)
				value = val

				if def.ValueType.IsDidFormat() {
					if o, err := rp.idResolver.ResolveIdent(context.Background(), val); err == nil {
						value = o.Handle.String()
					}
				}
			}
			labels = append(labels, ogre.LabelData{
				Color: def.GetColor(),
				Name:  fmt.Sprintf("%s%s", name, value),
			})
		}
	}

	ownerHandle := rp.pages.DisplayHandle(r.Context(), f.Did)
	authorHandle := rp.pages.DisplayHandle(r.Context(), issue.Did)

	avatarUrl := rp.pages.AvatarUrl(f.Did, "256")
	authorAvatarUrl := rp.pages.AvatarUrl(issue.Did, "256")

	status := "closed"
	if issue.Open {
		status = "open"
	}

	commentCount := len(issue.Comments)

	reactionCount, _ := db.GetReactionCount(rp.db, issue.AtUri())

	payload := ogre.IssueCardPayload{
		Type:            "issue",
		RepoName:        f.Name,
		OwnerHandle:     ownerHandle,
		AuthorHandle:    authorHandle,
		AvatarUrl:       avatarUrl,
		AuthorAvatarUrl: authorAvatarUrl,
		Title:           issue.Title,
		IssueNumber:     issue.IssueId,
		Status:          status,
		Labels:          labels,
		CommentCount:    commentCount,
		ReactionCount:   reactionCount,
		CreatedAt:       issue.Created.Format(time.RFC3339),
	}

	imageBytes, err := rp.ogreClient.RenderIssueCard(r.Context(), payload)
	if err != nil {
		rp.logger.Error("failed to render issue card", "err", err)
		http.Error(w, "failed to render issue card", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "image/png")
	w.Header().Set("Cache-Control", "public, max-age=3600")
	w.WriteHeader(http.StatusOK)
	_, err = w.Write(imageBytes)
	if err != nil {
		rp.logger.Error("failed to write issue card", "err", err)
		return
	}
}
