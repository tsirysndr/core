package repo

import (
	"log"
	"net/http"
	"sort"
	"time"

	"github.com/go-enry/go-enry/v2"
	"tangled.org/core/appview/db"
	"tangled.org/core/ogre"
	"tangled.org/core/orm"
	"tangled.org/core/types"
)

func (rp *Repo) Opengraph(w http.ResponseWriter, r *http.Request) {
	f, err := rp.repoResolver.Resolve(r)
	if err != nil {
		log.Println("failed to get repo and knot", err)
		return
	}

	ownerHandle := rp.pages.DisplayHandle(r.Context(), f.Did)

	avatarUrl := rp.pages.AvatarUrl(f.Did, "256")

	var languageStats []types.RepoLanguageDetails
	langs, err := db.GetRepoLanguages(
		rp.db,
		orm.FilterEq("repo_did", f.RepoDid),
		orm.FilterEq("is_default_ref", 1),
	)
	if err != nil {
		log.Printf("failed to get language stats from db: %v", err)
	} else if len(langs) > 0 {
		var total int64
		for _, l := range langs {
			total += l.Bytes
		}

		for _, l := range langs {
			percentage := float32(l.Bytes) / float32(total) * 100
			color := enry.GetColor(l.Language)
			languageStats = append(languageStats, types.RepoLanguageDetails{
				Name:       l.Language,
				Percentage: percentage,
				Color:      color,
			})
		}

		sort.Slice(languageStats, func(i, j int) bool {
			if languageStats[i].Name == enry.OtherLanguage {
				return false
			}
			if languageStats[j].Name == enry.OtherLanguage {
				return true
			}
			if languageStats[i].Percentage != languageStats[j].Percentage {
				return languageStats[i].Percentage > languageStats[j].Percentage
			}
			return languageStats[i].Name < languageStats[j].Name
		})
	}

	ogLanguages := []ogre.LanguageData{}
	for _, lang := range languageStats {
		if len(ogLanguages) >= 5 {
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
