package repos_indexer

import (
	"context"
	"os"
	"testing"

	"github.com/blevesearch/bleve/v2"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
)

func setupTestIndexer(t *testing.T) (*Indexer, func()) {
	t.Helper()

	tmpDir, err := os.MkdirTemp("", "repo_indexer_test")
	require.NoError(t, err)

	ix := NewIndexer(tmpDir)

	mapping, err := generateRepoIndexMapping()
	require.NoError(t, err)

	indexer, err := bleve.New(tmpDir, mapping)
	require.NoError(t, err)
	ix.indexer = indexer

	cleanup := func() {
		ix.indexer.Close()
		os.RemoveAll(tmpDir)
	}

	return ix, cleanup
}

func TestBasicIndexingAndSearch(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:          1,
			Did:         "did:plc:alice",
			Name:        "web-framework",
			Knot:        "example.com",
			Description: "A modern web framework for Go",
			Website:     "https://example.com/web-framework",
			Topics:      []string{"web", "framework", "golang"},
			RepoStats:   &models.RepoStats{Language: "Go"},
		},
		models.Repo{
			Id:          2,
			Did:         "did:plc:bob",
			Name:        "cli-tool",
			Knot:        "example.com",
			Description: "Command line utility for developers",
			Website:     "",
			Topics:      []string{"cli", "tool"},
			RepoStats:   &models.RepoStats{Language: "Rust"},
		},
		models.Repo{
			Id:          3,
			Did:         "did:plc:alice",
			Name:        "javascript-parser",
			Knot:        "example.com",
			Description: "Fast JavaScript parser",
			Website:     "",
			Topics:      []string{"javascript", "parser"},
			RepoStats:   &models.RepoStats{Language: "JavaScript"},
		},
	)
	require.NoError(t, err)

	// search by name
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"framework"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(1))

	// search by description
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"utility"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(2))

	// search by website
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"example.com/web-framework"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(1))
}

func TestLanguageFiltering(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:        1,
			Did:       "did:plc:alice",
			Name:      "go-project",
			RepoStats: &models.RepoStats{Language: "Go"},
		},
		models.Repo{
			Id:        2,
			Did:       "did:plc:bob",
			Name:      "rust-project",
			RepoStats: &models.RepoStats{Language: "Rust"},
		},
		models.Repo{
			Id:        3,
			Did:       "did:plc:alice",
			Name:      "another-go-project",
			RepoStats: &models.RepoStats{Language: "Go"},
		},
	)
	require.NoError(t, err)

	// filter by go language
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Language: "Go",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))

	// filter by rust language
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Language: "Rust",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(2))
}

func TestTopicExactMatching(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:        1,
			Did:       "did:plc:alice",
			Name:      "js-tool",
			Topics:    []string{"javascript", "tool"},
			RepoStats: &models.RepoStats{},
		},
		models.Repo{
			Id:        2,
			Did:       "did:plc:bob",
			Name:      "java-app",
			Topics:    []string{"java", "application"},
			RepoStats: &models.RepoStats{},
		},
		models.Repo{
			Id:        3,
			Did:       "did:plc:alice",
			Name:      "cli-tool",
			Topics:    []string{"cli", "tool"},
			RepoStats: &models.RepoStats{},
		},
	)
	require.NoError(t, err)

	// exact match for "javascript" topic
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Topics: []string{"javascript"},
		Page:   pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(1))

	// exact match for "tool" topic (should match repos 1 and 3)
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Topics: []string{"tool"},
		Page:   pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))
}

func TestTopicTextSearch(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:        1,
			Did:       "did:plc:alice",
			Name:      "js-tool",
			Topics:    []string{"JavaScript"},
			RepoStats: &models.RepoStats{},
		},
		models.Repo{
			Id:        2,
			Did:       "did:plc:bob",
			Name:      "java-app",
			Topics:    []string{"Java"},
			RepoStats: &models.RepoStats{},
		},
	)
	require.NoError(t, err)

	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"Java"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(2))
}

func TestNegatedFilters(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:          1,
			Did:         "did:plc:alice",
			Name:        "active-project",
			Description: "An active development project",
			Topics:      []string{"active"},
			RepoStats:   &models.RepoStats{Language: "Go"},
		},
		models.Repo{
			Id:          2,
			Did:         "did:plc:bob",
			Name:        "archived-project",
			Description: "An archived project",
			Topics:      []string{"archived"},
			RepoStats:   &models.RepoStats{Language: "Python"},
		},
		models.Repo{
			Id:          3,
			Did:         "did:plc:alice",
			Name:        "another-project",
			Description: "Another active project",
			Topics:      []string{"active"},
			RepoStats:   &models.RepoStats{Language: "Go"},
		},
	)
	require.NoError(t, err)

	// exclude archived topic
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		NegatedTopics: []string{"archived"},
		Page:          pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))

	// exclude keyword "archived"
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		NegatedKeywords: []string{"archived"},
		Page:            pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))

	// exclude phrase
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		NegatedPhrases: []string{"archived project"},
		Page:           pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))
}

func TestPagination(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	// index multiple repos
	var repos []models.Repo
	for i := 1; i <= 25; i++ {
		repos = append(repos, models.Repo{
			Id:        int64(i),
			Did:       "did:plc:alice",
			Name:      "project",
			Topics:    []string{"test"},
			RepoStats: &models.RepoStats{},
		})
	}
	err := ix.Index(ctx, repos...)
	require.NoError(t, err)

	// first page
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Topics: []string{"test"},
		Page:   pagination.Page{Limit: 10, Offset: 0},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(25), result.Total)
	assert.Len(t, result.Hits, 10)

	// second page
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Topics: []string{"test"},
		Page:   pagination.Page{Limit: 10, Offset: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(25), result.Total)
	assert.Len(t, result.Hits, 10)

	// third page - 5 items
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Topics: []string{"test"},
		Page:   pagination.Page{Limit: 10, Offset: 20},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(25), result.Total)
	assert.Len(t, result.Hits, 5)
}

func TestUpdateReindex(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	// initial index
	err := ix.Index(ctx, models.Repo{
		Id:          1,
		Did:         "did:plc:alice",
		Name:        "my-project",
		Description: "Initial description",
		Topics:      []string{"initial"},
		RepoStats:   &models.RepoStats{Language: "Go"},
	})
	require.NoError(t, err)

	// search for initial state
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"Initial"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)

	// update the repo
	err = ix.Index(ctx, models.Repo{
		Id:          1,
		Did:         "did:plc:alice",
		Name:        "my-project",
		Description: "Updated description",
		Topics:      []string{"updated"},
		RepoStats:   &models.RepoStats{Language: "Rust"},
	})
	require.NoError(t, err)

	// search for old description should return nothing
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"Initial"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(0), result.Total)

	// search for new description should work
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"Updated"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)

	// language should be updated
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Language: "Rust",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
}

func TestEmptyResults(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx, models.Repo{
		Id:        1,
		Did:       "did:plc:alice",
		Name:      "my-project",
		RepoStats: &models.RepoStats{},
	})
	require.NoError(t, err)

	// search for non-existent keyword
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"nonexistent"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(0), result.Total)
	assert.Empty(t, result.Hits)

	// search for non-existent language
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Language: "NonexistentLanguage",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(0), result.Total)
	assert.Empty(t, result.Hits)
}

func TestCombinedFilters(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:          1,
			Did:         "did:plc:alice",
			Name:        "web-server",
			Knot:        "example.com",
			Description: "A web server in Go",
			Topics:      []string{"web", "server"},
			RepoStats:   &models.RepoStats{Language: "Go"},
		},
		models.Repo{
			Id:          2,
			Did:         "did:plc:bob",
			Name:        "web-client",
			Knot:        "example.org",
			Description: "A web client in Rust",
			Topics:      []string{"web", "client"},
			RepoStats:   &models.RepoStats{Language: "Rust"},
		},
		models.Repo{
			Id:          3,
			Did:         "did:plc:alice",
			Name:        "cli-tool",
			Knot:        "example.com",
			Description: "A CLI tool in Go",
			Topics:      []string{"cli", "tool"},
			RepoStats:   &models.RepoStats{Language: "Go"},
		},
	)
	require.NoError(t, err)

	// combine language + topic + keyword
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Language: "Go",
		Topics:   []string{"web"},
		Keywords: []string{"server"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(1))

	// combine did + language
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Did:      "did:plc:alice",
		Language: "Go",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))

	// combine knot + language
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Knot:     "example.com",
		Language: "Go",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
	assert.Contains(t, result.Hits, int64(1))
	assert.Contains(t, result.Hits, int64(3))
}

func TestRepoWithoutLanguage(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:        1,
			Did:       "did:plc:alice",
			Name:      "project-with-language",
			RepoStats: &models.RepoStats{Language: "Go"},
		},
		models.Repo{
			Id:        2,
			Did:       "did:plc:bob",
			Name:      "project-without-language",
			RepoStats: &models.RepoStats{Language: ""},
		},
	)
	require.NoError(t, err)

	// search without language filter should return both
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"project"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)

	// language filter should only return repo with language
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Language: "Go",
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(1))
}

func TestRepoWithoutTopics(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:        1,
			Did:       "did:plc:alice",
			Name:      "project-with-topics",
			Topics:    []string{"cli", "tool"},
			RepoStats: &models.RepoStats{},
		},
		models.Repo{
			Id:        2,
			Did:       "did:plc:bob",
			Name:      "project-without-topics",
			Topics:    []string{},
			RepoStats: &models.RepoStats{},
		},
	)
	require.NoError(t, err)

	// topic filter should only return repo with topics
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Topics: []string{"cli"},
		Page:   pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(1))

	// general search should return both
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Keywords: []string{"project"},
		Page:     pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)
}

func TestDelete(t *testing.T) {
	ix, cleanup := setupTestIndexer(t)
	defer cleanup()

	ctx := context.Background()

	err := ix.Index(ctx,
		models.Repo{
			Id:        1,
			Did:       "did:plc:alice",
			Name:      "to-delete",
			RepoStats: &models.RepoStats{},
		},
		models.Repo{
			Id:        2,
			Did:       "did:plc:bob",
			Name:      "to-keep",
			RepoStats: &models.RepoStats{},
		},
	)
	require.NoError(t, err)

	// verify both exist
	result, err := ix.Search(ctx, models.RepoSearchOptions{
		Page: pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(2), result.Total)

	// delete repo 1
	err = ix.Delete(ctx, 1)
	require.NoError(t, err)

	// verify only one remains
	result, err = ix.Search(ctx, models.RepoSearchOptions{
		Page: pagination.Page{Limit: 10},
	})
	require.NoError(t, err)
	assert.Equal(t, uint64(1), result.Total)
	assert.Contains(t, result.Hits, int64(2))
}
