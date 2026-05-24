package pages

import (
	"fmt"
	"html"
	"net/http"
	"strings"
)

// Notice performs a hx-oob-swap to replace the content of an element with a message.
// Pass the id of the element and the message to display.
func (s *Pages) Notice(w http.ResponseWriter, id, msg string) {
	escaped := html.EscapeString(msg)
	markup := fmt.Sprintf(`<span id="%s" hx-swap-oob="innerHTML">%s</span>`, id, escaped)

	w.Header().Set("Content-Type", "text/html")
	w.WriteHeader(http.StatusOK)
	w.Write([]byte(markup))
}

func (s *Pages) NoticeHTML(w http.ResponseWriter, id string, trustedHTML string) {
	markup := fmt.Sprintf(`<span id="%s" hx-swap-oob="innerHTML">%s</span>`, id, trustedHTML)

	w.Header().Set("Content-Type", "text/html")
	w.WriteHeader(http.StatusOK)
	w.Write([]byte(markup))
}

func (s *Pages) NoticeHTMLWithClears(w http.ResponseWriter, id string, trustedHTML string, clearIDs ...string) {
	var b strings.Builder
	b.WriteString(fmt.Sprintf(`<span id="%s" hx-swap-oob="innerHTML">%s</span>`, id, trustedHTML))
	for _, clearID := range clearIDs {
		b.WriteString(fmt.Sprintf(`<span id="%s" hx-swap-oob="innerHTML"></span>`, clearID))
	}

	w.Header().Set("Content-Type", "text/html")
	w.WriteHeader(http.StatusOK)
	w.Write([]byte(b.String()))
}

// HxRefresh is a client-side full refresh of the page.
func (s *Pages) HxRefresh(w http.ResponseWriter) {
	w.Header().Set("HX-Refresh", "true")
	w.WriteHeader(http.StatusOK)
}

// HxRedirect is a full page reload with a new location.
func (s *Pages) HxRedirect(w http.ResponseWriter, location string) {
	w.Header().Set("HX-Redirect", location)
	w.WriteHeader(http.StatusOK)
}

// HxLocation is an SPA-style navigation to a new location.
func (s *Pages) HxLocation(w http.ResponseWriter, location string) {
	w.Header().Set("HX-Location", location)
	w.WriteHeader(http.StatusOK)
}
