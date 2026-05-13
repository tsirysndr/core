package pulls

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/config"
	"tangled.org/core/appview/models"
	"tangled.org/core/appview/pages"
)

const (
	resubmitTestOwnerDID  = "did:plc:boltless"
	resubmitTestRepoDID   = "did:plc:akshay"
	resubmitTestBranch    = "feature"
	resubmitTestSourceRev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
)

func newKnotmirrorStub(t *testing.T, hash string) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/xrpc/"+tangled.GitTempGetBranchNSID {
			t.Errorf("unexpected path %q", r.URL.Path)
			http.Error(w, "wrong path", http.StatusNotFound)
			return
		}
		repoQuery := r.URL.Query().Get("repo")
		if _, err := syntax.ParseDID(repoQuery); err != nil {
			t.Errorf("repo param %q is not a DID: %v", repoQuery, err)
			http.Error(w, "repo must be a DID", http.StatusBadRequest)
			return
		}
		if got := r.URL.Query().Get("name"); got != resubmitTestBranch {
			t.Errorf("name param = %q, want %q", got, resubmitTestBranch)
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(tangled.GitTempGetBranch_Output{
			Name: resubmitTestBranch,
			Hash: hash,
			When: time.Now().UTC().Format(time.RFC3339),
		})
	}))
}

func newPullsFromKnotURL(url string) *Pulls {
	return &Pulls{
		logger: slog.New(slog.NewTextHandler(io.Discard, nil)),
		config: &config.Config{
			KnotMirror: config.KnotMirrorConfig{Url: url},
		},
	}
}

func newForkPull(state models.PullState) (*models.Pull, *models.Repo, models.Stack) {
	sourceRepoDid := syntax.DID(resubmitTestRepoDID)
	pull := &models.Pull{
		State:        state,
		OwnerDid:     resubmitTestOwnerDID,
		TargetBranch: "main",
		Submissions: []*models.PullSubmission{
			{SourceRev: resubmitTestSourceRev},
		},
		PullSource: &models.PullSource{
			Branch:  resubmitTestBranch,
			RepoDid: &sourceRepoDid,
		},
	}
	repo := &models.Repo{RepoDid: resubmitTestRepoDID}
	stack := models.Stack{pull}
	return pull, repo, stack
}

func TestResubmitCheck_BranchAdvanced(t *testing.T) {
	srv := newKnotmirrorStub(t, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
	defer srv.Close()

	s := newPullsFromKnotURL(srv.URL)
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	pull, repo, stack := newForkPull(models.PullOpen)

	got := s.resubmitCheck(req, repo, pull, stack)
	if got != pages.ShouldResubmit {
		t.Errorf("resubmitCheck() = %v, want ShouldResubmit", got)
	}
}

func TestResubmitCheck_BranchUpToDate(t *testing.T) {
	srv := newKnotmirrorStub(t, resubmitTestSourceRev)
	defer srv.Close()

	s := newPullsFromKnotURL(srv.URL)
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	pull, repo, stack := newForkPull(models.PullOpen)

	got := s.resubmitCheck(req, repo, pull, stack)
	if got != pages.ShouldNotResubmit {
		t.Errorf("resubmitCheck() = %v, want ShouldNotResubmit", got)
	}
}

func TestResubmitCheck_MergedReturnsUnknown(t *testing.T) {
	s := newPullsFromKnotURL("http://unused")
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	pull, repo, stack := newForkPull(models.PullMerged)

	if got := s.resubmitCheck(req, repo, pull, stack); got != pages.Unknown {
		t.Errorf("resubmitCheck() = %v, want Unknown for merged pull", got)
	}
}

func TestResubmitCheck_PatchBasedReturnsUnknown(t *testing.T) {
	s := newPullsFromKnotURL("http://unused")
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	pull, repo, stack := newForkPull(models.PullOpen)
	pull.PullSource = nil

	if got := s.resubmitCheck(req, repo, pull, stack); got != pages.Unknown {
		t.Errorf("resubmitCheck() = %v, want Unknown for patch-based pull", got)
	}
}

func TestResubmitCheck_KnotUnreachableReturnsUnknown(t *testing.T) {
	s := newPullsFromKnotURL("http://127.0.0.1:1")
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	req := httptest.NewRequest(http.MethodGet, "/", nil).WithContext(ctx)
	pull, repo, stack := newForkPull(models.PullOpen)

	if got := s.resubmitCheck(req, repo, pull, stack); got != pages.Unknown {
		t.Errorf("resubmitCheck() = %v, want Unknown when knot unreachable", got)
	}
}

func TestResubmitCheck_NonForkUsesRepoDid(t *testing.T) {
	const targetRepoDID = "did:plc:scallop"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := r.URL.Query().Get("repo"); got != targetRepoDID {
			t.Errorf("repo param = %q, want %q for non-fork pull", got, targetRepoDID)
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(tangled.GitTempGetBranch_Output{
			Name: resubmitTestBranch,
			Hash: resubmitTestSourceRev,
			When: time.Now().UTC().Format(time.RFC3339),
		})
	}))
	defer srv.Close()

	s := newPullsFromKnotURL(srv.URL)
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	pull, _, stack := newForkPull(models.PullOpen)
	pull.PullSource.RepoDid = nil
	repo := &models.Repo{RepoDid: targetRepoDID}

	if got := s.resubmitCheck(req, repo, pull, stack); got != pages.ShouldNotResubmit {
		t.Errorf("resubmitCheck() = %v, want ShouldNotResubmit", got)
	}
}
