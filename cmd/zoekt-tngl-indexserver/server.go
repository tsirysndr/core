package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/atproto/identity"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"github.com/samber/lo"
	"tangled.org/core/repoident"
)

type IndexServer struct {
	cfg   *Config
	dir   identity.Directory
	queue *Queue
}

func NewIndexServer(cfg *Config) *IndexServer {
	return &IndexServer{
		cfg:   cfg,
		dir:   baseDir(cfg.PlcUrl),
		queue: NewQueue(cfg.IndexQueueSize),
	}
}

func (s *IndexServer) Run(ctx context.Context) {
	// // Start a goroutine which updates the queue with commits to index.
	// go func() {
	// }()

	for range s.cfg.IndexConcurrency {
		go s.processQueue(ctx)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/", s.handleHealth)
	mux.HandleFunc("/debug/metrics", s.handleMetrics)
	mux.HandleFunc("/debug/queue", s.handleDebugQueue)
	mux.HandleFunc("/admin/forceIndex", s.handleForceIndex)
	mux.HandleFunc("/admin/enqueueIndex", s.handleEnqueueIndex)
	if err := http.ListenAndServe(s.cfg.Listen, mux); err != nil {
		log.Fatal(err)
	}
}

func (s *IndexServer) handleHealth(w http.ResponseWriter, r *http.Request) {
	// Nothing to do. Just return 200
}

func (s *IndexServer) handleMetrics(w http.ResponseWriter, r *http.Request) {
	promhttp.Handler().ServeHTTP(w, r)
}

type branchName string

func (b branchName) Ref() string {
	return lo.Ternary(b == "HEAD", "HEAD", "refs/heads/"+string(b))
}

func (b *branchName) UnmarshalText(text []byte) error {
	name := branchName(text)
	if strings.HasPrefix(string(name), "refs/") {
		return fmt.Errorf("branch %q must be a short name, without the refs/ prefix", name)
	}
	if err := plumbing.ReferenceName(name.Ref()).Validate(); err != nil {
		return fmt.Errorf("branch %q isn't a valid ref: %w", name, err)
	}
	*b = name
	return nil
}

type objectID string

var objectIDPattern = regexp.MustCompile(`^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$`)

func (o *objectID) UnmarshalText(text []byte) error {
	if !objectIDPattern.Match(text) {
		return fmt.Errorf("%q isn't a sha1 or sha256 object id", text)
	}
	*o = objectID(text)
	return nil
}

type indexBranch struct {
	Name    branchName `json:"name"`
	Version objectID   `json:"version"`
}

type indexRequest struct {
	Repo     repoident.RepoDid `json:"repo"`
	Branches []indexBranch     `json:"branches"`
}

func decodeIndexRequest(r *http.Request) (indexRequest, error) {
	dec := json.NewDecoder(r.Body)
	dec.DisallowUnknownFields()
	var req indexRequest
	if err := dec.Decode(&req); err != nil {
		return indexRequest{}, err
	}
	if req.Repo == "" {
		return indexRequest{}, errors.New("index request has no repo did")
	}
	if len(req.Branches) == 0 {
		return indexRequest{}, fmt.Errorf("index request for %s has no branches", req.Repo)
	}
	return req, nil
}

func (s *IndexServer) handleDebugQueue(w http.ResponseWriter, r *http.Request) {
	for _, req := range s.queue.Snapshot() {
		fmt.Fprintln(w, req.Repo)
		for _, b := range req.Branches {
			fmt.Fprintf(w, "\t%s:\t%s\n", b.Name, b.Version)
		}
	}
}

func (s *IndexServer) handleEnqueueIndex(w http.ResponseWriter, r *http.Request) {
	route := "enqueueIndex"
	req, err := decodeIndexRequest(r)
	if err != nil {
		log.Printf("Error decoding index request: %v", err)
		http.Error(w, "JSON parser error", http.StatusBadRequest)
		s.incrementRequestsTotal(r.Method, route, http.StatusBadRequest)
		return
	}

	if !s.queue.Enqueue(req) {
		// queue full: reject so the producer retries later
		http.Error(w, "index queue full", http.StatusServiceUnavailable)
		s.incrementRequestsTotal(r.Method, route, http.StatusServiceUnavailable)
		return
	}

	w.WriteHeader(http.StatusAccepted)
	s.incrementRequestsTotal(r.Method, route, http.StatusAccepted)
}

func (s *IndexServer) handleForceIndex(w http.ResponseWriter, r *http.Request) {
	route := "index"
	req, err := decodeIndexRequest(r)
	if err != nil {
		log.Printf("Error decoding index request: %v", err)
		http.Error(w, "JSON parser error", http.StatusBadRequest)
		s.incrementRequestsTotal(r.Method, route, http.StatusBadRequest)
		return
	}

	if err := gitIndex(r.Context(), s.cfg, s.dir, req); err != nil {
		s.respondWithError(w, r.Method, route, err)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{
		"success": true,
	})

	s.incrementRequestsTotal(r.Method, route, http.StatusOK)
}

func (s *IndexServer) respondWithError(w http.ResponseWriter, method, route string, err error) {
	responseCode := http.StatusInternalServerError

	log.Print(err)
	s.incrementRequestsTotal(method, route, responseCode)

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(responseCode)
	response := map[string]any{
		"Success": false,
		"Error":   err.Error(),
	}

	_ = json.NewEncoder(w).Encode(response)
}

func (s *IndexServer) incrementRequestsTotal(method, route string, responseCode int) {
	requestsTotal.With(prometheus.Labels{"code": strconv.Itoa(responseCode), "method": method, "route": route}).Inc()
}

func (s *IndexServer) processQueue(ctx context.Context) {
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}

		req, ok := s.queue.Pop()
		if !ok {
			time.Sleep(time.Second)
			continue
		}

		if err := gitIndex(ctx, s.cfg, s.dir, req); err != nil {
			log.Printf("indexing repo %s failed: %v", req.Repo, err)
		}
	}
}
