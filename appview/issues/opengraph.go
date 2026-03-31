package issues

import (
	"context"
	"fmt"
	"log"
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
		log.Println("failed to get repo and knot", err)
		return
	}

	issue, ok := r.Context().Value("issue").(*models.Issue)
	if !ok {
		log.Println("issue not found in context")
		http.Error(w, "issue not found", http.StatusNotFound)
		return
	}

	labelDefs, err := db.GetLabelDefinitions(
		rp.db,
		orm.FilterIn("at_uri", f.Labels),
		orm.FilterContains("scope", tangled.RepoIssueNSID),
	)
	if err != nil {
		log.Println("failed to fetch label definitions")
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

	var ownerHandle string
	owner, err := rp.idResolver.ResolveIdent(context.Background(), f.Did)
	if err != nil {
		ownerHandle = f.Did
	} else {
		ownerHandle = owner.Handle.String()
	}

	avatarUrl := rp.pages.AvatarUrl(ownerHandle, "256")

	status := "closed"
	if issue.Open {
		status = "open"
	}

	commentCount := len(issue.Comments)

	reactionCount, _ := db.GetReactionCount(rp.db, issue.AtUri())

	payload := ogre.IssueCardPayload{
		Type:          "issue",
		RepoName:      f.Name,
		OwnerHandle:   ownerHandle,
		AvatarUrl:     avatarUrl,
		Title:         issue.Title,
		IssueNumber:   issue.IssueId,
		Status:        status,
		Labels:        labels,
		CommentCount:  commentCount,
		ReactionCount: reactionCount,
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
