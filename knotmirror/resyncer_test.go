package knotmirror

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/models"
)

func uploadPackAdvert(capabilities string) string {
	return "001e# service=git-upload-pack\n" +
		"0000" +
		"0000000000000000000000000000000000000000 capabilities^{}\x00" + capabilities + "\n" +
		"0000"
}

func TestCheckKnotObjectFormat(t *testing.T) {
	const gitContentType = "application/x-git-upload-pack-advertisement"

	tests := []struct {
		name          string
		status        int
		contentType   string
		body          string
		wantFormat    models.ObjectFormat
		wantErr       bool
		wantRateLimit bool
	}{
		{
			name:        "sha256 repo is detected",
			status:      http.StatusOK,
			contentType: gitContentType,
			body:        uploadPackAdvert("multi_ack thin-pack side-band-64k ofs-delta object-format=sha256 agent=git/2.45.0"),
			wantFormat:  models.ObjectFormatSHA256,
		},
		{
			name:        "explicit sha1 repo stays on sha1",
			status:      http.StatusOK,
			contentType: gitContentType,
			body:        uploadPackAdvert("multi_ack thin-pack side-band-64k ofs-delta object-format=sha1 agent=git/2.45.0"),
			wantFormat:  models.ObjectFormatSHA1,
		},
		{
			name:        "advertisement without object-format defaults to sha1",
			status:      http.StatusOK,
			contentType: gitContentType,
			body:        uploadPackAdvert("multi_ack thin-pack side-band-64k ofs-delta agent=git/2.34.0"),
			wantFormat:  models.ObjectFormatSHA1,
		},
		{
			name:        "sha256 detected with content-type parameters",
			status:      http.StatusOK,
			contentType: gitContentType + "; charset=utf-8",
			body:        uploadPackAdvert("object-format=sha256"),
			wantFormat:  models.ObjectFormatSHA256,
		},
		{
			name:          "rate limited knot",
			status:        http.StatusTooManyRequests,
			wantErr:       true,
			wantRateLimit: true,
		},
		{
			name:    "missing repo on knot",
			status:  http.StatusNotFound,
			wantErr: true,
		},
		{
			name:        "non git content-type",
			status:      http.StatusOK,
			contentType: "text/html",
			body:        "<html>not a git server</html>",
			wantErr:     true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var gotPath string
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				gotPath = r.URL.Path
				if tt.contentType != "" {
					w.Header().Set("Content-Type", tt.contentType)
				}
				w.WriteHeader(tt.status)
				io.WriteString(w, tt.body)
			}))
			defer srv.Close()

			r := &Resyncer{
				logger:     slog.New(slog.NewTextHandler(io.Discard, nil)),
				cfg:        &config.Config{},
				httpClient: srv.Client(),
			}
			repo := &models.Repo{
				RepoDid:    "did:plc:boltless",
				KnotDomain: srv.URL,
			}

			format, err := r.checkKnot(context.Background(), repo)

			if tt.wantErr {
				if err == nil {
					t.Fatalf("expected error, got format %q", format)
				}
				if format != "" {
					t.Errorf("expected empty format on error, got %q", format)
				}
				if got := isRateLimitError(err); got != tt.wantRateLimit {
					t.Errorf("isRateLimitError = %v, want %v", got, tt.wantRateLimit)
				}
				return
			}

			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if format != tt.wantFormat {
				t.Errorf("format = %q, want %q", format, tt.wantFormat)
			}
			if !strings.HasSuffix(gotPath, "/info/refs") {
				t.Errorf("expected upstream request to /info/refs, got %q", gotPath)
			}
		})
	}
}
