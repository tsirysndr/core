package git

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	gogit "github.com/go-git/go-git/v5"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"github.com/stretchr/testify/suite"
)

type TagSuite struct {
	suite.Suite
	*RepoSuite
}

func TestTagSuite(t *testing.T) {
	t.Parallel()
	suite.Run(t, new(TagSuite))
}

func (s *TagSuite) SetupTest() {
	s.RepoSuite = NewRepoSuite(s.T())
}

func (s *TagSuite) TearDownTest() {
	s.RepoSuite.cleanup()
}

func (s *TagSuite) setupRepoWithTags() {
	s.init()

	// create commits for tagging
	commit1 := s.commitFile("file1.txt", "content 1", "Add file1")
	commit2 := s.commitFile("file2.txt", "content 2", "Add file2")
	commit3 := s.commitFile("file3.txt", "content 3", "Add file3")
	commit4 := s.commitFile("file4.txt", "content 4", "Add file4")
	commit5 := s.commitFile("file5.txt", "content 5", "Add file5")

	// create annotated tags
	s.createAnnotatedTag(
		"v1.0.0",
		commit1,
		"Tagger One",
		"tagger1@example.com",
		"Release version 1.0.0\n\nThis is the first stable release.",
		s.baseTime.Add(1*time.Hour),
	)

	s.createAnnotatedTag(
		"v1.1.0",
		commit2,
		"Tagger Two",
		"tagger2@example.com",
		"Release version 1.1.0",
		s.baseTime.Add(2*time.Hour),
	)

	// create lightweight tags
	s.createLightweightTag("v2.0.0", commit3)
	s.createLightweightTag("v2.1.0", commit4)

	// create another annotated tag
	s.createAnnotatedTag(
		"v3.0.0",
		commit5,
		"Tagger Three",
		"tagger3@example.com",
		"Major version 3.0.0\n\nBreaking changes included.",
		s.baseTime.Add(3*time.Hour),
	)
}

func (s *TagSuite) TestTags_All() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)

	// we created 5 tags total (3 annotated, 2 lightweight)
	assert.Len(s.T(), tags, 5, "expected 5 tags")

	// verify tags are sorted by creation date (newest first)
	expectedAnnotated := map[string]bool{
		"v1.0.0": true,
		"v1.1.0": true,
		"v3.0.0": true,
	}

	expectedLightweight := map[string]bool{
		"v2.0.0": true,
		"v2.1.0": true,
	}

	for _, tag := range tags {
		if expectedAnnotated[tag.Name] {
			// annotated tags should have tagger info
			assert.NotEmpty(s.T(), tag.Tagger.Name, "annotated tag %s should have tagger name", tag.Name)
			assert.NotEmpty(s.T(), tag.Message, "annotated tag %s should have message", tag.Name)
		} else if expectedLightweight[tag.Name] {
			// lightweight tags won't have tagger info or message (they'll have empty values)
		} else {
			s.T().Errorf("unexpected tag name: %s", tag.Name)
		}
	}
}

func (s *TagSuite) TestTags_WithLimit() {
	s.setupRepoWithTags()

	tests := []struct {
		name          string
		limit         int
		expectedCount int
	}{
		{
			name:          "limit 1",
			limit:         1,
			expectedCount: 1,
		},
		{
			name:          "limit 2",
			limit:         2,
			expectedCount: 2,
		},
		{
			name:          "limit 3",
			limit:         3,
			expectedCount: 3,
		},
		{
			name:          "limit 10 (more than available)",
			limit:         10,
			expectedCount: 5,
		},
	}

	for _, tt := range tests {
		s.Run(tt.name, func() {
			tags, err := s.repo.Tags(&TagsOptions{
				Limit: tt.limit,
			})
			require.NoError(s.T(), err)
			assert.Len(s.T(), tags, tt.expectedCount, "expected %d tags", tt.expectedCount)
		})
	}
}

func (s *TagSuite) TestTags_WithOffset() {
	s.setupRepoWithTags()

	tests := []struct {
		name          string
		offset        int
		expectedCount int
	}{
		{
			name:          "offset 0",
			offset:        0,
			expectedCount: 5,
		},
		{
			name:          "offset 1",
			offset:        1,
			expectedCount: 4,
		},
		{
			name:          "offset 2",
			offset:        2,
			expectedCount: 3,
		},
		{
			name:          "offset 4",
			offset:        4,
			expectedCount: 1,
		},
		{
			name:          "offset 5 (all skipped)",
			offset:        5,
			expectedCount: 0,
		},
		{
			name:          "offset 10 (more than available)",
			offset:        10,
			expectedCount: 0,
		},
	}

	for _, tt := range tests {
		s.Run(tt.name, func() {
			tags, err := s.repo.Tags(&TagsOptions{
				Offset: tt.offset,
			})
			require.NoError(s.T(), err)
			assert.Len(s.T(), tags, tt.expectedCount, "expected %d tags", tt.expectedCount)
		})
	}
}

func (s *TagSuite) TestTags_WithLimitAndOffset() {
	s.setupRepoWithTags()

	tests := []struct {
		name          string
		limit         int
		offset        int
		expectedCount int
	}{
		{
			name:          "limit 2, offset 0",
			limit:         2,
			offset:        0,
			expectedCount: 2,
		},
		{
			name:          "limit 2, offset 1",
			limit:         2,
			offset:        1,
			expectedCount: 2,
		},
		{
			name:          "limit 2, offset 3",
			limit:         2,
			offset:        3,
			expectedCount: 2,
		},
		{
			name:          "limit 2, offset 4",
			limit:         2,
			offset:        4,
			expectedCount: 1,
		},
		{
			name:          "limit 3, offset 2",
			limit:         3,
			offset:        2,
			expectedCount: 3,
		},
		{
			name:          "limit 10, offset 3",
			limit:         10,
			offset:        3,
			expectedCount: 2,
		},
	}

	for _, tt := range tests {
		s.Run(tt.name, func() {
			tags, err := s.repo.Tags(&TagsOptions{
				Limit:  tt.limit,
				Offset: tt.offset,
			})
			require.NoError(s.T(), err)
			assert.Len(s.T(), tags, tt.expectedCount, "expected %d tags", tt.expectedCount)
		})
	}
}

func (s *TagSuite) TestTags_EmptyRepo() {
	repoPath := filepath.Join(s.tempDir, "empty-repo")

	_, err := gogit.PlainInit(repoPath, false)
	require.NoError(s.T(), err)

	gitRepo, err := PlainOpen(repoPath)
	require.NoError(s.T(), err)

	tags, err := gitRepo.Tags(nil)
	require.NoError(s.T(), err)

	if tags != nil {
		assert.Empty(s.T(), tags, "expected no tags in empty repo")
	}
}

func (s *TagSuite) TestTags_Pagination() {
	s.setupRepoWithTags()

	allTags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)
	assert.Len(s.T(), allTags, 5, "expected 5 tags")

	pageSize := 2
	var paginatedTags []object.Tag

	for offset := 0; offset < len(allTags); offset += pageSize {
		tags, err := s.repo.Tags(&TagsOptions{
			Limit:  pageSize,
			Offset: offset,
		})
		require.NoError(s.T(), err)
		paginatedTags = append(paginatedTags, tags...)
	}

	assert.Len(s.T(), paginatedTags, len(allTags), "pagination should return all tags")

	for i := range allTags {
		assert.Equal(s.T(), allTags[i].Name, paginatedTags[i].Name,
			"tag at index %d differs", i)
	}
}

func (s *TagSuite) TestTags_VerifyAnnotatedTagFields() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)

	var v1Tag *object.Tag
	for i := range tags {
		if tags[i].Name == "v1.0.0" {
			v1Tag = &tags[i]
			break
		}
	}

	require.NotNil(s.T(), v1Tag, "v1.0.0 tag not found")

	assert.Equal(s.T(), "Tagger One", v1Tag.Tagger.Name, "tagger name should match")
	assert.Equal(s.T(), "tagger1@example.com", v1Tag.Tagger.Email, "tagger email should match")

	assert.Equal(s.T(), "Release version 1.0.0\n\nThis is the first stable release.\n",
		v1Tag.Message, "tag message should match")

	assert.Equal(s.T(), plumbing.TagObject, v1Tag.TargetType,
		"target type should be CommitObject")

	assert.False(s.T(), v1Tag.Hash.IsZero(), "tag hash should be set")

	assert.False(s.T(), v1Tag.Target.IsZero(), "target hash should be set")
}

func (s *TagSuite) TestTags_NilOptions() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)
	assert.Len(s.T(), tags, 5, "nil options should return all tags")
}

func (s *TagSuite) TestTags_ZeroLimitAndOffset() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(&TagsOptions{
		Limit:  0,
		Offset: 0,
	})
	require.NoError(s.T(), err)
	assert.Len(s.T(), tags, 5, "zero limit should return all tags")
}

func (s *TagSuite) TestTags_OrderedNewestFirst() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)
	require.Len(s.T(), tags, 5)

	// v3.0.0 has the latest tagger date (baseTime+3h), should be first
	assert.Equal(s.T(), "v3.0.0", tags[0].Name, "newest tag should be first")
}

func (s *TagSuite) TestTags_LatestWithLimit1() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(&TagsOptions{Limit: 1})
	require.NoError(s.T(), err)
	require.Len(s.T(), tags, 1)

	assert.Equal(s.T(), "v3.0.0", tags[0].Name, "limit=1 should return the newest tag")
}

func (s *TagSuite) TestTags_Pattern() {
	s.setupRepoWithTags()

	v1tag, err := s.repo.Tags(&TagsOptions{
		Pattern: "refs/tags/v1.0.0",
	})

	require.NoError(s.T(), err)
	assert.Len(s.T(), v1tag, 1, "expected 1 tag")
}

func (s *TagSuite) TestTags_PatternGlob() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(&TagsOptions{
		Pattern: "refs/tags/v1.*",
	})
	require.NoError(s.T(), err)
	require.Len(s.T(), tags, 2, "glob pattern refs/tags/v1.* should match 2 tags")

	names := map[string]bool{}
	for _, t := range tags {
		names[t.Name] = true
	}
	assert.True(s.T(), names["v1.0.0"], "v1.0.0 should match pattern")
	assert.True(s.T(), names["v1.1.0"], "v1.1.0 should match pattern")
}

func (s *TagSuite) TestTags_PatternNoMatch() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(&TagsOptions{
		Pattern: "refs/tags/v9.*",
	})
	require.NoError(s.T(), err)
	assert.Empty(s.T(), tags, "non-matching pattern should return no tags")
}

func (s *TagSuite) TestTags_VerifyLightweightTagFields() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)

	var v2Tag *object.Tag
	for i := range tags {
		if tags[i].Name == "v2.0.0" {
			v2Tag = &tags[i]
			break
		}
	}
	require.NotNil(s.T(), v2Tag, "v2.0.0 tag not found")

	assert.Empty(s.T(), v2Tag.Tagger.Name, "lightweight tag should have no tagger name")
	assert.Empty(s.T(), v2Tag.Tagger.Email, "lightweight tag should have no tagger email")
	assert.True(s.T(), v2Tag.Tagger.When.IsZero(), "lightweight tag should have zero tagger date")
	// For a lightweight tag %(contents:subject) returns the commit subject, not a tag annotation
	assert.Equal(s.T(), "Add file3", v2Tag.Message, "lightweight tag message should be the commit subject")
	assert.False(s.T(), v2Tag.Hash.IsZero(), "lightweight tag hash should be the commit hash")
	assert.Equal(s.T(), plumbing.CommitObject, v2Tag.TargetType, "lightweight tag should resolve to a commit")
}

func (s *TagSuite) TestTags_SubjectOnlyMessage() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)

	var v11Tag *object.Tag
	for i := range tags {
		if tags[i].Name == "v1.1.0" {
			v11Tag = &tags[i]
			break
		}
	}
	require.NotNil(s.T(), v11Tag, "v1.1.0 tag not found")

	// v1.1.0 was created with a subject-only message (no body paragraph)
	assert.Equal(s.T(), "Release version 1.1.0", v11Tag.Message,
		"subject-only tag message should equal the subject line")
}

func (s *TagSuite) TestTags_FullOrdering() {
	s.setupRepoWithTags()

	tags, err := s.repo.Tags(nil)
	require.NoError(s.T(), err)
	require.Len(s.T(), tags, 5)

	// Annotated tags carry explicit tagger dates (baseTime+3h, +2h, +1h) so they
	// sort ahead of lightweight tags whose commits have no explicit author date.
	assert.Equal(s.T(), "v3.0.0", tags[0].Name, "v3.0.0 should be newest (baseTime+3h)")
	assert.Equal(s.T(), "v1.1.0", tags[1].Name, "v1.1.0 should be second (baseTime+2h)")
	assert.Equal(s.T(), "v1.0.0", tags[2].Name, "v1.0.0 should be third (baseTime+1h)")

	// Lightweight tags v2.0.0 and v2.1.0 have zero-time commits and sort to the
	// end; their relative order is not guaranteed.
	lastName := map[string]bool{tags[3].Name: true, tags[4].Name: true}
	assert.True(s.T(), lastName["v2.0.0"], "v2.0.0 should be in the last two positions")
	assert.True(s.T(), lastName["v2.1.0"], "v2.1.0 should be in the last two positions")
}

func (s *TagSuite) TestTags_ForEachRefError() {
	s.setupRepoWithTags()

	// Remove .git so the underlying git command fails.
	err := os.RemoveAll(filepath.Join(s.repo.path, ".git"))
	require.NoError(s.T(), err)

	_, err = s.repo.Tags(nil)
	assert.Error(s.T(), err, "Tags should return an error when the git command fails")
}

func (s *TagSuite) TestParseTagRecord_BodyOnly() {
	// When subject is empty and body is non-empty the else branch sets message = body.
	fields := []string{
		"v1.0.0",    // tagName
		"abc123",    // objectHash
		"tag",       // objectType
		"def456",    // targetHash
		"commit",    // targetType
		"Tagger",    // taggerName
		"<t@t.com>", // taggerEmail
		"0",         // taggerDate
		"",          // subject — empty
		"body text", // body — non-empty
		"",          // signature
	}
	line := strings.Join(fields, fieldSeparator)
	tag, ok, err := parseTagRecord(line)
	require.NoError(s.T(), err)
	require.True(s.T(), ok)
	assert.Equal(s.T(), "body text", tag.Message, "body-only message should equal the body field")
}

func (s *TagSuite) TestParseTagRecord_ShortRecord() {
	// A record with fewer than 6 fields must be skipped without error.
	short := strings.Join([]string{"v1.0.0", "abc123", "tag", "def456"}, fieldSeparator)
	tag, ok, err := parseTagRecord(short)
	require.NoError(s.T(), err)
	assert.False(s.T(), ok, "short record should be skipped")
	assert.Equal(s.T(), object.Tag{}, tag)
}

func (s *TagSuite) TestParseTagRecord_InvalidObjectType() {
	// A record whose objecttype field is unrecognised must surface an error.
	fields := []string{
		"v1.0.0",       // tagName
		"abc123",       // objectHash
		"invalid_type", // objectType — not a valid git object type
		"def456",       // targetHash
		"commit",       // targetType
		"Tagger",       // taggerName
		"<t@t.com>",    // taggerEmail
		"0",            // taggerDate
		"subject",      // subject
		"body",         // body
		"",             // signature
	}
	line := strings.Join(fields, fieldSeparator)
	_, ok, err := parseTagRecord(line)
	assert.Error(s.T(), err, "invalid object type should return an error")
	assert.False(s.T(), ok)
}
