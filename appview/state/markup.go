package state

import "net/http"

func (s *State) MarkdownPreview(w http.ResponseWriter, r *http.Request) {
	body := r.FormValue("body")
	s.pages.MarkdownPreviewFragment(w, body)
}
