package state

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"tangled.org/core/xrpc"
)

const maxBlobSize = 1_000_000

func (s *State) MarkdownPreview(w http.ResponseWriter, r *http.Request) {
	body := r.FormValue("body")
	s.pages.MarkdownPreviewFragment(w, body)
}

// MarkupUpload proxies an image upload to the user's PDS via uploadBlob and
// returns the blob ref plus a blob+at://<did>/<cid> URI. The browser can't call
// uploadBlob directly (the DPoP key lives server-side), so this is the bridge.
func (s *State) MarkupUpload(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "MarkupUpload")

	user := s.oauth.GetMultiAccountUser(r)
	if user == nil {
		writeUploadError(w, http.StatusUnauthorized, "not logged in")
		return
	}
	l = l.With("did", user.Did)

	contentType := r.Header.Get("Content-Type")
	if !strings.HasPrefix(contentType, "image/") {
		writeUploadError(w, http.StatusUnsupportedMediaType, "only image uploads are allowed")
		return
	}

	// cap the body at the lexicon's maxSize (MaxBytesReader errors past it)
	r.Body = http.MaxBytesReader(w, r.Body, maxBlobSize)
	defer r.Body.Close()

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		l.Error("failed to get authorized client", "err", err)
		writeUploadError(w, http.StatusBadGateway, "failed to connect to your PDS")
		return
	}

	// pre-warm DPoP nonce
	if _, err := comatproto.ServerGetSession(r.Context(), client); err != nil {
		l.Error("failed to pre-warm session", "err", err)
		writeUploadError(w, http.StatusInternalServerError, "failed to pre-warm session")
		return
	}

	resp, err := xrpc.RepoUploadBlob(r.Context(), client, r.Body, contentType)
	if err != nil {
		// MaxBytesReader's over-limit error surfaces through LexDo
		if strings.Contains(err.Error(), "request body too large") {
			l.Warn("upload exceeds size limit")
			writeUploadError(w, http.StatusRequestEntityTooLarge, "image too large (max 1MB)")
			return
		}
		l.Error("failed to upload blob", "err", err)
		writeUploadError(w, http.StatusBadGateway, "failed to upload image to your PDS")
		return
	}

	blob := resp.Blob
	cid := blob.Ref.String()
	l.Info("uploaded blob", "cid", cid, "size", blob.Size)

	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(map[string]any{
		"blob": blob,
		"did":  user.Did,
		"uri":  fmt.Sprintf("blob+at://%s/%s", user.Did, cid),
	}); err != nil {
		l.Error("failed to encode upload response", "err", err)
	}
}

func writeUploadError(w http.ResponseWriter, status int, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"error": msg})
}
