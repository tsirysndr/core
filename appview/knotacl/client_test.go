package knotacl

import (
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"sync"
	"testing"

	"tangled.org/core/api/tangled"
)

func testLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

const (
	testOwner   = "did:plc:akshay"
	testCollab  = "did:plc:boltless"
	testRepoDid = "did:plc:limpet"
	testStrange = "did:plc:scallop"
)

type recordingKnot struct {
	mu       sync.Mutex
	requests []string
	handler  func(w http.ResponseWriter, r *http.Request)
}

func (k *recordingKnot) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	k.mu.Lock()
	k.requests = append(k.requests, r.URL.String())
	k.mu.Unlock()
	k.handler(w, r)
}

func (k *recordingKnot) calls() []string {
	k.mu.Lock()
	defer k.mu.Unlock()
	return slices.Clone(k.requests)
}

func devClientFor(t *testing.T, handler func(w http.ResponseWriter, r *http.Request)) (*Client, *recordingKnot, string) {
	t.Helper()
	knot := &recordingKnot{handler: handler}
	srv := httptest.NewServer(knot)
	t.Cleanup(srv.Close)
	host := strings.TrimPrefix(srv.URL, "http://")
	return NewClient(true, testLogger()), knot, host
}

func memberPage(items []string, cursor string) tangled.KnotListMembers_Output {
	out := tangled.KnotListMembers_Output{
		Items: mapSlice(items, func(d string) *tangled.KnotListMembers_ListItem {
			return &tangled.KnotListMembers_ListItem{Subject: d, AddedBy: testOwner, CreatedAt: "2026-06-03T00:00:00Z"}
		}),
	}
	if cursor != "" {
		out.Cursor = &cursor
	}
	return out
}

func collabPage(items []string, cursor string) tangled.RepoListCollaborators_Output {
	out := tangled.RepoListCollaborators_Output{
		Items: mapSlice(items, func(d string) *tangled.RepoListCollaborators_ListItem {
			return &tangled.RepoListCollaborators_ListItem{Subject: d, AddedBy: testOwner, CreatedAt: "2026-06-03T00:00:00Z"}
		}),
	}
	if cursor != "" {
		out.Cursor = &cursor
	}
	return out
}

func TestGetKnotMembers_SinglePage(t *testing.T) {
	c, _, host := devClientFor(t, func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(memberPage([]string{testCollab, testOwner, testCollab}, ""))
	})

	got, err := c.GetKnotMembers(context.Background(), host)
	if err != nil {
		t.Fatalf("GetKnotMembers: %v", err)
	}
	want := []string{testOwner, testCollab}
	if !slices.Equal(got, want) {
		t.Errorf("members = %v, want sorted+deduped %v", got, want)
	}
}

func TestGetKnotMembers_Paginates(t *testing.T) {
	c, knot, host := devClientFor(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("cursor") == "" {
			json.NewEncoder(w).Encode(memberPage([]string{testOwner}, "page2"))
			return
		}
		json.NewEncoder(w).Encode(memberPage([]string{testCollab}, ""))
	})

	got, err := c.GetKnotMembers(context.Background(), host)
	if err != nil {
		t.Fatalf("GetKnotMembers: %v", err)
	}
	if want := []string{testOwner, testCollab}; !slices.Equal(got, want) {
		t.Errorf("members = %v, want union %v", got, want)
	}
	calls := knot.calls()
	if len(calls) != 2 {
		t.Fatalf("calls = %d, want 2 pages", len(calls))
	}
	if !strings.Contains(calls[1], "cursor=page2") {
		t.Errorf("second call %q did not carry the page-1 cursor", calls[1])
	}
}

func TestGetKnotMembers_KnotDown(t *testing.T) {
	c, _, host := devClientFor(t, func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "boom", http.StatusInternalServerError)
	})

	_, err := c.GetKnotMembers(context.Background(), host)
	if err == nil {
		t.Fatal("want error when knot is down; the Client must surface it for the Service to swallow")
	}
}

func TestGetRepoCollaborators_PassesRepoDidAsSubject(t *testing.T) {
	c, knot, host := devClientFor(t, func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(collabPage([]string{testCollab}, ""))
	})

	got, err := c.GetRepoCollaborators(context.Background(), host, testRepoDid)
	if err != nil {
		t.Fatalf("GetRepoCollaborators: %v", err)
	}
	if want := []string{testCollab}; !slices.Equal(got, want) {
		t.Errorf("collaborators = %v, want %v", got, want)
	}
	if calls := knot.calls(); len(calls) != 1 || !strings.Contains(calls[0], "subject="+strings.ReplaceAll(testRepoDid, ":", "%3A")) {
		t.Errorf("subject param missing repo DID: %v", calls)
	}
}

func TestDrainStopsOnRepeatedCursor(t *testing.T) {
	c, knot, host := devClientFor(t, func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(memberPage([]string{testRepoDid}, "stuck"))
	})

	got, err := c.GetKnotMembers(context.Background(), host)
	if err != nil {
		t.Fatalf("GetKnotMembers: %v", err)
	}
	if want := []string{testRepoDid}; !slices.Equal(got, want) {
		t.Errorf("members = %v, want %v after dedup", got, want)
	}
	if calls := len(knot.calls()); calls != 2 {
		t.Errorf("calls = %d, want 2; a knot echoing the same cursor must halt at once, not page to the cap", calls)
	}
}

func TestDrainStopsAtPageCap(t *testing.T) {
	c, knot, host := devClientFor(t, func(w http.ResponseWriter, r *http.Request) {
		cursor := r.URL.Query().Get("cursor")
		json.NewEncoder(w).Encode(memberPage([]string{testCollab}, cursor+"x"))
	})

	got, err := c.GetKnotMembers(context.Background(), host)
	if err != nil {
		t.Fatalf("GetKnotMembers: %v", err)
	}
	if want := []string{testCollab}; !slices.Equal(got, want) {
		t.Errorf("members = %v, want %v after dedup", got, want)
	}
	if calls := len(knot.calls()); calls != maxListPages {
		t.Errorf("calls = %d, want the %d-page cap to halt an ever-advancing cursor", calls, maxListPages)
	}
}
