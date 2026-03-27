// heavily inspired by gitea's model (basically copy-pasted)
package repos_indexer

import (
	"context"
	"errors"
	"log"
	"os"

	"github.com/blevesearch/bleve/v2"
	"github.com/blevesearch/bleve/v2/analysis/analyzer/custom"
	"github.com/blevesearch/bleve/v2/analysis/token/camelcase"
	"github.com/blevesearch/bleve/v2/analysis/token/lowercase"
	"github.com/blevesearch/bleve/v2/analysis/token/ngram"
	"github.com/blevesearch/bleve/v2/analysis/token/unicodenorm"
	"github.com/blevesearch/bleve/v2/analysis/tokenizer/unicode"
	"github.com/blevesearch/bleve/v2/index/upsidedown"
	"github.com/blevesearch/bleve/v2/mapping"
	"github.com/blevesearch/bleve/v2/search/query"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/indexer/base36"
	bleveutil "tangled.org/core/appview/indexer/bleve"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pagination"
	tlog "tangled.org/core/log"
)

const (
	repoIndexerAnalyzer = "repoIndexer"
	repoIndexerDocType  = "repoIndexerDocType"

	unicodeNormalizeName = "unicodeNormalize"

	// Bump this when the index mapping changes to trigger a rebuild.
	repoIndexerVersion = 5
)

type Indexer struct {
	indexer bleve.Index
	path    string
}

func NewIndexer(indexDir string) *Indexer {
	return &Indexer{
		path: indexDir,
	}
}

// Init initializes the indexer
func (ix *Indexer) Init(ctx context.Context, e db.Execer) {
	l := tlog.FromContext(ctx)
	existed, err := ix.intialize(ctx)
	if err != nil {
		log.Fatalln("failed to initialize repo indexer", err)
	}
	if !existed {
		l.Debug("Populating the repo indexer")
		err := PopulateIndexer(ctx, ix, e)
		if err != nil {
			log.Fatalln("failed to populate repo indexer", err)
		}
	}

	count, _ := ix.indexer.DocCount()
	l.Info("Initialized the repo indexer", "docCount", count)
}

func generateRepoIndexMapping() (mapping.IndexMapping, error) {
	mapping := bleve.NewIndexMapping()
	docMapping := bleve.NewDocumentMapping()

	textFieldMapping := bleve.NewTextFieldMapping()
	textFieldMapping.Store = false
	textFieldMapping.IncludeInAll = false

	keywordFieldMapping := bleve.NewKeywordFieldMapping()
	keywordFieldMapping.Store = false
	keywordFieldMapping.IncludeInAll = false

	// case-insensitive keyword field for language and topics
	caseInsensitiveKeywordMapping := bleve.NewTextFieldMapping()
	caseInsensitiveKeywordMapping.Store = false
	caseInsensitiveKeywordMapping.IncludeInAll = false
	caseInsensitiveKeywordMapping.Analyzer = "keyword_lowercase"

	// trigram field for partial repo name matching
	trigramFieldMapping := bleve.NewTextFieldMapping()
	trigramFieldMapping.Store = false
	trigramFieldMapping.IncludeInAll = false
	trigramFieldMapping.Analyzer = "trigram"

	// text fields
	docMapping.AddFieldMappingsAt("name", textFieldMapping)
	docMapping.AddFieldMappingsAt("name_trigram", trigramFieldMapping)
	docMapping.AddFieldMappingsAt("description", textFieldMapping)
	docMapping.AddFieldMappingsAt("website", textFieldMapping)
	docMapping.AddFieldMappingsAt("topics", textFieldMapping)

	// keyword fields
	docMapping.AddFieldMappingsAt("language", caseInsensitiveKeywordMapping)
	docMapping.AddFieldMappingsAt("topics_exact", caseInsensitiveKeywordMapping)
	docMapping.AddFieldMappingsAt("did", keywordFieldMapping)
	docMapping.AddFieldMappingsAt("knot", keywordFieldMapping)
	docMapping.AddFieldMappingsAt("repo_at", keywordFieldMapping)

	err := mapping.AddCustomTokenFilter(unicodeNormalizeName, map[string]any{
		"type": unicodenorm.Name,
		"form": unicodenorm.NFC,
	})
	if err != nil {
		return nil, err
	}

	err = mapping.AddCustomTokenFilter("edgeNgram3", map[string]any{
		"type": ngram.Name,
		"min":  2.0,
		"max":  3.0,
	})
	if err != nil {
		return nil, err
	}

	err = mapping.AddCustomAnalyzer(repoIndexerAnalyzer, map[string]any{
		"type":          custom.Name,
		"char_filters":  []string{},
		"tokenizer":     unicode.Name,
		"token_filters": []string{unicodeNormalizeName, camelcase.Name, lowercase.Name},
	})
	if err != nil {
		return nil, err
	}

	err = mapping.AddCustomAnalyzer("keyword_lowercase", map[string]any{
		"type":          custom.Name,
		"char_filters":  []string{},
		"tokenizer":     "single",
		"token_filters": []string{lowercase.Name},
	})
	if err != nil {
		return nil, err
	}

	err = mapping.AddCustomAnalyzer("trigram", map[string]any{
		"type":          custom.Name,
		"char_filters":  []string{},
		"tokenizer":     "single",
		"token_filters": []string{lowercase.Name, "edgeNgram3"},
	})
	if err != nil {
		return nil, err
	}

	mapping.DefaultAnalyzer = repoIndexerAnalyzer
	mapping.AddDocumentMapping(repoIndexerDocType, docMapping)
	mapping.AddDocumentMapping("_all", bleve.NewDocumentDisabledMapping())
	mapping.DefaultMapping = bleve.NewDocumentDisabledMapping()

	return mapping, nil
}

func (ix *Indexer) intialize(ctx context.Context) (bool, error) {
	if ix.indexer != nil {
		return false, errors.New("indexer is already initialized")
	}

	indexer, err := openIndexer(ctx, ix.path, repoIndexerVersion)
	if err != nil {
		return false, err
	}
	if indexer != nil {
		ix.indexer = indexer
		return true, nil
	}

	mapping, err := generateRepoIndexMapping()
	if err != nil {
		return false, err
	}
	indexer, err = bleve.New(ix.path, mapping)
	if err != nil {
		return false, err
	}
	indexer.SetInternal([]byte("mapping_version"), []byte{byte(repoIndexerVersion)})

	ix.indexer = indexer

	return false, nil
}

func openIndexer(ctx context.Context, path string, version int) (bleve.Index, error) {
	l := tlog.FromContext(ctx)
	indexer, err := bleve.Open(path)
	if err != nil {
		if errors.Is(err, upsidedown.IncompatibleVersion) {
			l.Info("Indexer was built with a previous version of bleve, deleting and rebuilding")
			return nil, os.RemoveAll(path)
		}
		return nil, nil
	}

	storedVersion, _ := indexer.GetInternal([]byte("mapping_version"))
	if storedVersion == nil || int(storedVersion[0]) != version {
		l.Info("Indexer mapping version changed, deleting and rebuilding")
		indexer.Close()
		return nil, os.RemoveAll(path)
	}

	return indexer, nil
}

func PopulateIndexer(ctx context.Context, ix *Indexer, e db.Execer) error {
	l := tlog.FromContext(ctx)
	count := 0

	err := pagination.IterateAll(
		func(page pagination.Page) ([]models.Repo, error) {
			return db.GetReposPaginated(e, page)
		},
		func(repos []models.Repo) error {
			count += len(repos)
			return ix.Index(ctx, repos...)
		},
	)

	l.Info("repos indexed", "count", count)
	return err
}

type repoData struct {
	ID          int64    `json:"id"`
	RepoAt      string   `json:"repo_at"`
	Did         string   `json:"did"`
	Name        string   `json:"name"`
	NameTrigram string   `json:"name_trigram"`
	Description string   `json:"description"`
	Website     string   `json:"website"`
	Topics      []string `json:"topics"`
	TopicsExact []string `json:"topics_exact"`
	Knot        string   `json:"knot"`
	Language    string   `json:"language"`
}

func makeRepoData(repo *models.Repo) *repoData {
	return &repoData{
		ID:          repo.Id,
		RepoAt:      repo.RepoAt().String(),
		Did:         repo.Did,
		Name:        repo.Name,
		NameTrigram: repo.Name,
		Description: repo.Description,
		Website:     repo.Website,
		Topics:      repo.Topics,
		TopicsExact: repo.Topics,
		Knot:        repo.Knot,
		Language:    repo.RepoStats.Language,
	}
}

// Type returns the document type, for bleve's mapping.Classifier interface.
func (r *repoData) Type() string {
	return repoIndexerDocType
}

type SearchResult struct {
	Hits  []int64
	Total uint64
}

const maxBatchSize = 20

func (ix *Indexer) Index(ctx context.Context, repos ...models.Repo) error {
	batch := bleveutil.NewFlushingBatch(ix.indexer, maxBatchSize)
	for _, repo := range repos {
		repoData := makeRepoData(&repo)
		if err := batch.Index(base36.Encode(repo.Id), repoData); err != nil {
			return err
		}
	}
	return batch.Flush()
}

func (ix *Indexer) Delete(ctx context.Context, repoID int64) error {
	return ix.indexer.Delete(base36.Encode(repoID))
}

func (ix *Indexer) Search(ctx context.Context, opts models.RepoSearchOptions) (*SearchResult, error) {
	var musts []query.Query
	var mustNots []query.Query

	for _, keyword := range opts.Keywords {
		musts = append(musts, bleve.NewDisjunctionQuery(
			bleveutil.MatchAndQuery("name", keyword, repoIndexerAnalyzer, 0),
			bleveutil.MatchAndQuery("name_trigram", keyword, "trigram", 0),
			bleveutil.MatchAndQuery("description", keyword, repoIndexerAnalyzer, 0),
			bleveutil.MatchAndQuery("website", keyword, repoIndexerAnalyzer, 0),
			bleveutil.MatchAndQuery("topics", keyword, repoIndexerAnalyzer, 0),
		))
	}

	for _, phrase := range opts.Phrases {
		musts = append(musts, bleve.NewDisjunctionQuery(
			bleveutil.MatchPhraseQuery("name", phrase, repoIndexerAnalyzer),
			bleveutil.MatchPhraseQuery("description", phrase, repoIndexerAnalyzer),
			bleveutil.MatchPhraseQuery("website", phrase, repoIndexerAnalyzer),
			bleveutil.MatchPhraseQuery("topics", phrase, repoIndexerAnalyzer),
		))
	}

	for _, keyword := range opts.NegatedKeywords {
		mustNots = append(mustNots, bleve.NewDisjunctionQuery(
			bleveutil.MatchAndQuery("name", keyword, repoIndexerAnalyzer, 0),
			bleveutil.MatchAndQuery("description", keyword, repoIndexerAnalyzer, 0),
			bleveutil.MatchAndQuery("website", keyword, repoIndexerAnalyzer, 0),
			bleveutil.MatchAndQuery("topics", keyword, repoIndexerAnalyzer, 0),
		))
	}

	for _, phrase := range opts.NegatedPhrases {
		mustNots = append(mustNots, bleve.NewDisjunctionQuery(
			bleveutil.MatchPhraseQuery("name", phrase, repoIndexerAnalyzer),
			bleveutil.MatchPhraseQuery("description", phrase, repoIndexerAnalyzer),
			bleveutil.MatchPhraseQuery("website", phrase, repoIndexerAnalyzer),
			bleveutil.MatchPhraseQuery("topics", phrase, repoIndexerAnalyzer),
		))
	}

	// keyword filters
	if opts.Language != "" {
		musts = append(musts, bleveutil.MatchAndQuery("language", opts.Language, "keyword_lowercase", 0))
	}

	if opts.Knot != "" {
		musts = append(musts, bleveutil.KeywordFieldQuery("knot", opts.Knot))
	}

	if opts.Did != "" {
		musts = append(musts, bleveutil.KeywordFieldQuery("did", opts.Did))
	}

	for _, topic := range opts.Topics {
		musts = append(musts, bleveutil.MatchAndQuery("topics_exact", topic, "keyword_lowercase", 0))
	}

	for _, topic := range opts.NegatedTopics {
		mustNots = append(mustNots, bleveutil.MatchAndQuery("topics_exact", topic, "keyword_lowercase", 0))
	}

	indexerQuery := bleve.NewBooleanQuery()
	if len(musts) == 0 {
		musts = append(musts, bleve.NewMatchAllQuery())
	}
	indexerQuery.AddMust(musts...)
	indexerQuery.AddMustNot(mustNots...)
	searchReq := bleve.NewSearchRequestOptions(indexerQuery, opts.Page.Limit, opts.Page.Offset, false)
	res, err := ix.indexer.SearchInContext(ctx, searchReq)
	if err != nil {
		return nil, nil
	}
	ret := &SearchResult{
		Total: res.Total,
		Hits:  make([]int64, len(res.Hits)),
	}
	for i, hit := range res.Hits {
		id, err := base36.Decode(hit.ID)
		if err != nil {
			return nil, err
		}
		ret.Hits[i] = id
	}
	return ret, nil
}
