package repo

import (
	"log"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/ogre"
)

const MaxOpengraphLanguageKinds = 4

func (rp *Repo) Opengraph(w http.ResponseWriter, r *http.Request) {
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		log.Println("failed to get repo and knot", err)
		return
	}

	ownerHandle := rp.pages.DisplayHandle(r.Context(), f.Did)

	avatarUrl := rp.pages.AvatarUrl(f.Did, "256")

	langs, err := rp.getLanguageInfo(r.Context(), syntax.DID(f.RepoDid), "")
	if err != nil {
		log.Printf("failed to get language stats from knotmirror: %v", err)
	}
	languageStats := makeLanguageStats(langs)

	ogLanguages := []ogre.LanguageData{}
	for _, lang := range languageStats {
		if len(ogLanguages) > MaxOpengraphLanguageKinds {
			break
		}
		ogLanguages = append(ogLanguages, ogre.LanguageData{
			Color:      lang.Color,
			Percentage: lang.Percentage,
		})
	}

	payload := ogre.RepositoryCardPayload{
		Type:        "repository",
		RepoName:    f.Name,
		OwnerHandle: ownerHandle,
		Stars:       f.RepoStats.StarCount,
		Pulls:       f.RepoStats.PullCount.Open,
		Issues:      f.RepoStats.IssueCount.Open,
		CreatedAt:   f.Created.Format(time.RFC3339),
		AvatarUrl:   avatarUrl,
		Languages:   ogLanguages,
	}

	imageBytes, err := rp.ogreClient.RenderRepositoryCard(r.Context(), payload)
	if err != nil {
		log.Println("failed to render repository card", err)
		http.Error(w, "failed to render repository card", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "image/png")
	w.Header().Set("Cache-Control", "public, max-age=3600")
	w.WriteHeader(http.StatusOK)
	_, err = w.Write(imageBytes)
	if err != nil {
		log.Println("failed to write repository card", err)
		return
	}
}
