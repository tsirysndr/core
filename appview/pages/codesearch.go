package pages

import "sort"

// helper functions to render search match highlights

// mergeIntervals sorts half-open rune intervals and merges overlapping/adjacent ones.
func mergeIntervals(in [][2]int) [][2]int {
	if len(in) < 2 {
		return in
	}
	sort.Slice(in, func(i, j int) bool { return in[i][0] < in[j][0] })
	out := in[:1]
	for _, iv := range in[1:] {
		last := &out[len(out)-1]
		if iv[0] <= last[1] {
			if iv[1] > last[1] {
				last[1] = iv[1]
			}
			continue
		}
		out = append(out, iv)
	}
	return out
}

// spanRunes splits runes into alternating unmatched/matched ChunkSpans using the
// (sorted, merged) match intervals. Returns nil for an empty line.
func spanRunes(runes []rune, intervals [][2]int) []ChunkSpan {
	if len(runes) == 0 {
		return nil
	}
	if len(intervals) == 0 {
		return []ChunkSpan{{Text: string(runes)}}
	}
	var spans []ChunkSpan
	pos := 0
	for _, iv := range intervals {
		if iv[0] > pos {
			spans = append(spans, ChunkSpan{Text: string(runes[pos:iv[0]])})
		}
		spans = append(spans, ChunkSpan{Text: string(runes[iv[0]:iv[1]]), Match: true})
		pos = iv[1]
	}
	if pos < len(runes) {
		spans = append(spans, ChunkSpan{Text: string(runes[pos:])})
	}
	return spans
}
