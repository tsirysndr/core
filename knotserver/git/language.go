package git

import (
	"context"
	"io"
	"path"
	"strings"

	"github.com/go-enry/go-enry/v2"
	"github.com/go-git/go-git/v5/plumbing/object"
)

type LangBreakdown map[string]int64

const (
	langContentLimit = 16 * 1024       // read up to 16 KB for language detection
	langSizeLimit    = 1 * 1024 * 1024 // skip content read for blobs over 1 MB
)

func (g *GitRepo) AnalyzeLanguages(ctx context.Context) (LangBreakdown, error) {
	sizes := make(map[string]int64)
	err := g.Walk(ctx, "", func(node object.TreeEntry, parent *object.Tree, root string) error {
		filepath := path.Join(root, node.Name)

		if enry.IsVendor(filepath) || enry.IsDocumentation(filepath) ||
			enry.IsDotFile(filepath) || enry.IsConfiguration(filepath) {
			return nil
		}

		blob, err := object.GetBlob(g.r.Storer, node.Hash)
		if err != nil {
			return nil
		}
		sz := blob.Size

		var content []byte
		if sz <= langSizeLimit {
			r, err := blob.Reader()
			if err != nil {
				return nil
			}
			content, _ = io.ReadAll(io.LimitReader(r, langContentLimit))
			r.Close()
		}

		if enry.IsGenerated(filepath, content) ||
			enry.IsBinary(content) ||
			strings.HasSuffix(filepath, "bun.lock") {
			return nil
		}

		language := analyzeLanguage(node, content)
		if group := enry.GetLanguageGroup(language); group != "" {
			language = group
		}

		langType := enry.GetLanguageType(language)
		if langType != enry.Programming && langType != enry.Markup {
			return nil
		}

		sizes[language] += sz

		return nil
	})

	if err != nil {
		return nil, err
	}

	return sizes, nil
}

func analyzeLanguage(node object.TreeEntry, content []byte) string {
	language, ok := enry.GetLanguageByExtension(node.Name)
	if ok {
		return language
	}

	language, ok = enry.GetLanguageByFilename(node.Name)
	if ok {
		return language
	}

	if len(content) == 0 {
		return enry.OtherLanguage
	}

	return enry.GetLanguage(node.Name, content)
}
