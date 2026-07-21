package xrpc

import (
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/sourcegraph/zoekt"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/codesearch"
	"tangled.org/core/appview/pagination"
)

func (x *Xrpc) SearchSearchCode(w http.ResponseWriter, r *http.Request) {
	l := x.Logger.With("handler", "SearchSearchCode")

	if x.CodeSearch == nil {
		writeError(w, notImplementedError("code search is not configured"), http.StatusNotImplemented)
		return
	}

	q := r.URL.Query()
	rawQuery := strings.TrimSpace(q.Get("q"))
	if rawQuery == "" {
		writeError(w, badRequestError("missing required parameter: q"), http.StatusBadRequest)
		return
	}

	// scope by the repo's own did (meta.did) and language; post-filtered below
	var scope []string
	var repoFilter syntax.DID
	if raw := strings.TrimSpace(q.Get("repoDid")); raw != "" {
		repoDid, err := syntax.ParseDID(raw)
		if err != nil {
			writeError(w, badRequestError("invalid repoDid"), http.StatusBadRequest)
			return
		}
		repoFilter = repoDid
		scope = append(scope, fmt.Sprintf("meta.did:%s", repoDid))
	}
	if lang := strings.TrimSpace(q.Get("lang")); lang != "" {
		scope = append(scope, fmt.Sprintf("lang:%s", lang))
	}
	queryStr := strings.TrimSpace(strings.Join(scope, " ") + " " + rawQuery)

	page := pagination.Page{Limit: 50}
	if s := q.Get("limit"); s != "" {
		if n, err := strconv.Atoi(s); err == nil && n > 0 && n <= 100 {
			page.Limit = n
		}
	}
	if s := q.Get("cursor"); s != "" {
		if n, err := strconv.Atoi(s); err == nil && n >= 0 {
			page.Offset = n
		}
	}

	res, err := x.CodeSearch.Search(r.Context(), queryStr, page)
	if err != nil {
		var repoErr *codesearch.RepoOnlyError
		if errors.As(err, &repoErr) {
			writeError(w, badRequestError("query only filters by repo name; use repo search instead"), http.StatusBadRequest)
			return
		}
		l.Error("code search failed", "err", err, "query", queryStr)
		writeError(w, errInternal, http.StatusInternalServerError)
		return
	}

	// meta.did is best-effort, so drop anything that isn't the requested repo
	filtered := res.Results
	if repoFilter != "" {
		filtered = filtered[:0]
		for _, item := range res.Results {
			if item.RepoDID == repoFilter {
				filtered = append(filtered, item)
			}
		}
	}

	results := make([]*tangled.TempSearchSearchCode_FileResult, 0, len(filtered))
	for _, item := range filtered {
		fr := &tangled.TempSearchSearchCode_FileResult{
			RepoDid: item.RepoDID.String(),
			Path:    item.FilePath,
		}
		if item.Language != "" {
			lang := item.Language
			fr.Language = &lang
		}
		for _, c := range item.Chunks {
			fr.Chunks = append(fr.Chunks, &tangled.TempSearchSearchCode_Chunk{
				Content:    c.Content,
				LineStart:  int64(c.ContentStartLine),
				Highlights: chunkHighlights(c.Content, c.ContentStartLine, c.Ranges),
			})
		}
		results = append(results, fr)
	}

	out := &tangled.TempSearchSearchCode_Output{Results: results}
	if res.HasMore {
		cursor := strconv.Itoa(page.Offset + page.Limit)
		out.Cursor = &cursor
	}

	x.writeJSON(w, out)
}

// chunkHighlights maps zoekt (line, rune-column) ranges to byte-offset ranges
// within the chunk's content string
func chunkHighlights(content string, startLine int, ranges []zoekt.Range) []*tangled.TempSearchSearchCode_Highlight {
	if startLine < 1 {
		startLine = 1
	}
	lines := strings.SplitAfter(content, "\n")
	lineOffset := make([]int, len(lines))
	off := 0
	for i, ln := range lines {
		lineOffset[i] = off
		off += len(ln)
	}

	// byteAt maps a 1-based (line, rune column) to a byte offset in content
	byteAt := func(lineNum, runeCol int) int {
		idx := lineNum - startLine
		if idx < 0 || idx >= len(lines) {
			return -1
		}
		col := runeCol - 1
		if col < 0 {
			col = 0
		}
		r := 0
		for bi := range lines[idx] {
			if r == col {
				return lineOffset[idx] + bi
			}
			r++
		}
		return lineOffset[idx] + len(strings.TrimSuffix(lines[idx], "\n"))
	}

	var out []*tangled.TempSearchSearchCode_Highlight
	for _, rg := range ranges {
		s := byteAt(int(rg.Start.LineNumber), int(rg.Start.Column))
		e := byteAt(int(rg.End.LineNumber), int(rg.End.Column))
		if s < 0 || e < 0 || e <= s {
			continue
		}
		out = append(out, &tangled.TempSearchSearchCode_Highlight{Start: int64(s), End: int64(e)})
	}
	return out
}
