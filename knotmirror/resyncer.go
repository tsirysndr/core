package knotmirror

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math/rand"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/knotmirror/config"
	"tangled.org/core/knotmirror/db"
	"tangled.org/core/knotmirror/knotstream"
	"tangled.org/core/knotmirror/models"
	"tangled.org/core/log"
)

type Resyncer struct {
	logger  *slog.Logger
	db      *sql.DB
	gitm    GitMirrorManager
	cfg     *config.Config
	indexer *knotstream.ParallelScheduler

	claimJobMu sync.Mutex

	runningJobs   map[syntax.DID]context.CancelFunc
	runningJobsMu sync.Mutex

	repoFetchTimeout    time.Duration
	manualResyncTimeout time.Duration
	parallelism         int

	knotBackoff   map[string]time.Time
	knotBackoffMu sync.RWMutex

	httpClient *http.Client
}

func NewResyncer(l *slog.Logger, db *sql.DB, gitm GitMirrorManager, indexer *knotstream.ParallelScheduler, cfg *config.Config) *Resyncer {
	return &Resyncer{
		logger:  log.SubLogger(l, "resyncer"),
		db:      db,
		gitm:    gitm,
		cfg:     cfg,
		indexer: indexer,

		runningJobs: make(map[syntax.DID]context.CancelFunc),

		repoFetchTimeout:    cfg.GitRepoFetchTimeout,
		manualResyncTimeout: 30 * time.Minute,
		parallelism:         cfg.ResyncParallelism,

		knotBackoff: make(map[string]time.Time),

		httpClient: &http.Client{Timeout: 30 * time.Second},
	}
}

func (r *Resyncer) Start(ctx context.Context) {
	for i := 0; i < r.parallelism; i++ {
		go r.runResyncWorker(ctx, i)
	}
}

func (r *Resyncer) runResyncWorker(ctx context.Context, workerID int) {
	l := r.logger.With("worker", workerID)
	for {
		select {
		case <-ctx.Done():
			l.Info("resync worker shutting down", "error", ctx.Err())
			return
		default:
		}
		repoDid, found, err := r.claimResyncJob(ctx)
		if err != nil {
			l.Error("failed to claim resync job", "error", err)
			time.Sleep(time.Second)
			continue
		}
		if !found {
			time.Sleep(time.Second)
			continue
		}
		l.Info("processing resync", "did", repoDid)
		if err := r.resyncRepo(ctx, repoDid); err != nil {
			l.Error("resync failed", "did", repoDid, "error", err)
		}
	}
}

func (r *Resyncer) registerRunning(repo syntax.DID, cancel context.CancelFunc) {
	r.runningJobsMu.Lock()
	defer r.runningJobsMu.Unlock()

	if _, exists := r.runningJobs[repo]; exists {
		return
	}
	r.runningJobs[repo] = cancel
}

func (r *Resyncer) unregisterRunning(repo syntax.DID) {
	r.runningJobsMu.Lock()
	defer r.runningJobsMu.Unlock()

	delete(r.runningJobs, repo)
}

func (r *Resyncer) CancelResyncJob(repo syntax.DID) {
	r.runningJobsMu.Lock()
	defer r.runningJobsMu.Unlock()

	cancel, ok := r.runningJobs[repo]
	if !ok {
		return
	}
	delete(r.runningJobs, repo)
	cancel()
}

// TriggerResyncJob manually triggers the resync job
func (r *Resyncer) TriggerResyncJob(ctx context.Context, repoDid syntax.DID) error {
	repo, err := db.GetRepoByRepoDid(ctx, r.db, repoDid)
	if err != nil {
		return fmt.Errorf("failed to get repo: %w", err)
	}
	if repo == nil {
		return fmt.Errorf("repo not found: %s", repoDid)
	}

	if repo.State == models.RepoStateResyncing {
		return fmt.Errorf("repo already resyncing")
	}

	repo.State = models.RepoStatePending
	repo.RetryAfter = -1 // resyncer will prioritize this

	if err := db.UpsertRepo(ctx, r.db, repo); err != nil {
		return fmt.Errorf("updating repo state to pending %w", err)
	}
	return nil
}

func (r *Resyncer) claimResyncJob(ctx context.Context) (syntax.DID, bool, error) {
	// use mutex to prevent duplicated jobs
	r.claimJobMu.Lock()
	defer r.claimJobMu.Unlock()

	var repoDid syntax.DID
	now := time.Now().Unix()
	if err := r.db.QueryRowContext(ctx,
		`update repos
		set state = $1
		where repo_did = (
			select repo_did from repos
			where state in ($2, $3, $4)
			and (retry_after = -1 or retry_after = 0 or retry_after < $5)
			order by
				(retry_after = -1) desc,
				(retry_after = 0) desc,
				retry_after
			limit 1
		)
		returning repo_did
		`,
		models.RepoStateResyncing,
		models.RepoStatePending, models.RepoStateDesynchronized, models.RepoStateError,
		now,
	).Scan(&repoDid); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return "", false, nil
		}
		return "", false, err
	}

	return repoDid, true, nil
}

func (r *Resyncer) resyncRepo(ctx context.Context, repoDid syntax.DID) error {
	// ctx, span := tracer.Start(ctx, "resyncRepo")
	// span.SetAttributes(attribute.String("aturi", repoAt))
	// defer span.End()

	resyncsStarted.Inc()
	startTime := time.Now()

	jobCtx, cancel := context.WithCancel(ctx)
	r.registerRunning(repoDid, cancel)
	defer r.unregisterRunning(repoDid)

	success, err := r.doResync(jobCtx, repoDid)
	if !success {
		resyncsFailed.Inc()
		resyncDuration.Observe(time.Since(startTime).Seconds())
		return r.handleResyncFailure(ctx, repoDid, err)
	}

	resyncsCompleted.Inc()
	resyncDuration.Observe(time.Since(startTime).Seconds())
	return nil
}

func (r *Resyncer) doResync(ctx context.Context, repoDid syntax.DID) (bool, error) {
	// ctx, span := tracer.Start(ctx, "doResync")
	// span.SetAttributes(attribute.String("aturi", repoAt))
	// defer span.End()

	repo, err := db.GetRepoByRepoDid(ctx, r.db, repoDid)
	if err != nil {
		return false, fmt.Errorf("failed to get repo: %w", err)
	}
	if repo == nil { // untracked repo, skip
		return false, nil
	}

	r.knotBackoffMu.RLock()
	backoffUntil, inBackoff := r.knotBackoff[repo.KnotDomain]
	r.knotBackoffMu.RUnlock()
	if inBackoff && time.Now().Before(backoffUntil) {
		return false, nil
	}

	// HACK: check knot reachability with short timeout before running actual fetch.
	// This is crucial as git-cli doesn't support http connection timeout.
	// `http.lowSpeedTime` is only applied _after_ the connection.
	format, err := r.checkKnot(ctx, repo)
	if err != nil {
		if isRateLimitError(err) {
			r.knotBackoffMu.Lock()
			r.knotBackoff[repo.KnotDomain] = time.Now().Add(10 * time.Second)
			r.knotBackoffMu.Unlock()
			return false, nil
		}
		// TODO: suspend repo on 404. KnotStream updates will change the repo state back online
		return false, fmt.Errorf("knot unreachable: %w", err)
	}

	if format == models.ObjectFormatSHA256 {
		return r.suspendUnsupported(ctx, repo)
	}

	timeout := r.repoFetchTimeout
	if repo.RetryAfter == -1 {
		timeout = r.manualResyncTimeout
	}
	fetchCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	if err := r.gitm.Sync(fetchCtx, repo); err != nil {
		return false, err
	}

	// request index to zoekt server
	// NOTE: indexing after full git resync is bad design. We are doing _after_ the sync because knotstream event doesn't include repository refs.
	// NOTE: and zoekt indexer should directly subscribe to the knot. remove this when we have knotrelay.
	if r.cfg.Search.ZoektUrl != "" {
		go func() {
			idxCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
			defer cancel()
			defaultBranch, err := r.gitm.DefaultBranch(idxCtx, repo)
			if err != nil {
				r.logger.Warn("resolving default branch for indexing failed", "did", repo.RepoDid, "error", err)
				return
			}
			if err := r.requestIndex(idxCtx, repo.RepoDid, []branch{defaultBranch}); err != nil {
				r.logger.Warn("requesting zoekt index failed", "did", repo.RepoDid, "err", err)
			}
		}()
	}

	// queue repo_stats_update job
	r.indexer.AddTask(context.TODO(), &knotstream.Task{Key: repo.RepoDid.String()})

	// repo.GitRev = <processed git.refUpdate revision>
	// repo.RepoSha = <sha256 sum of git refs>
	repo.State = models.RepoStateActive
	repo.ErrorMsg = ""
	repo.RetryCount = 0
	repo.RetryAfter = 0
	if err := db.UpsertRepo(ctx, r.db, repo); err != nil {
		return false, fmt.Errorf("updating repo state to active %w", err)
	}
	return true, nil
}

type knotStatusError struct {
	StatusCode int
}

func (ke *knotStatusError) Error() string {
	return fmt.Sprintf("request failed with status code (HTTP %d)", ke.StatusCode)
}

func isRateLimitError(err error) bool {
	var knotErr *knotStatusError
	if errors.As(err, &knotErr) {
		return knotErr.StatusCode == http.StatusTooManyRequests
	}
	return false
}

func (r *Resyncer) checkKnot(ctx context.Context, repo *models.Repo) (models.ObjectFormat, error) {
	repoUrl, err := makeRepoRemoteUrl(repo.KnotDomain, repo.RepoIdentifier(), r.cfg.KnotUseSSL)
	if err != nil {
		return "", err
	}

	repoUrl += "/info/refs?service=git-upload-pack"

	r.logger.Debug("checking knot reachability", "url", repoUrl)

	req, err := http.NewRequestWithContext(ctx, "GET", repoUrl, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("User-Agent", "git/2.x")
	req.Header.Set("Accept", "*/*")

	resp, err := r.httpClient.Do(req)
	if err != nil {
		var uerr *url.Error
		if errors.As(err, &uerr) {
			return "", fmt.Errorf("request failed: %w", uerr.Unwrap())
		}
		return "", fmt.Errorf("request failed: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return "", &knotStatusError{resp.StatusCode}
	}

	// check if target is git server
	ct := resp.Header.Get("Content-Type")
	if !strings.Contains(ct, "application/x-git-upload-pack-advertisement") {
		return "", fmt.Errorf("unexpected content-type: %s", ct)
	}

	advertisement, err := io.ReadAll(io.LimitReader(resp.Body, 64*1024))
	if err != nil {
		return "", fmt.Errorf("reading upload-pack advertisement: %w", err)
	}
	if bytes.Contains(advertisement, []byte("object-format=sha256")) {
		return models.ObjectFormatSHA256, nil
	}

	return models.ObjectFormatSHA1, nil
}

func (r *Resyncer) suspendUnsupported(ctx context.Context, repo *models.Repo) (bool, error) {
	if err := r.gitm.Delete(repo); err != nil {
		r.logger.Warn("failed to remove local clone of suspended repo", "did", repo.RepoDid, "err", err)
	}

	repo.State = models.RepoStateSuspended
	repo.ErrorMsg = "unsupported sha256 object format"
	repo.RetryCount = 0
	repo.RetryAfter = 0
	if err := db.UpsertRepo(ctx, r.db, repo); err != nil {
		return false, fmt.Errorf("suspending sha256 repo: %w", err)
	}

	r.logger.Info("suspended sha256 repo, reads forwarded to knot", "did", repo.RepoDid, "knot", repo.KnotDomain)
	return true, nil
}

func (r *Resyncer) handleResyncFailure(ctx context.Context, repoDid syntax.DID, err error) error {
	r.logger.Debug("handleResyncFailure", "at_uri", repoDid, "err", err)
	var state models.RepoState
	var errMsg string
	if err == nil {
		state = models.RepoStateDesynchronized
		errMsg = ""
	} else {
		state = models.RepoStateError
		errMsg = err.Error()
	}

	repo, err := db.GetRepoByRepoDid(ctx, r.db, repoDid)
	if err != nil {
		return fmt.Errorf("failed to get repo: %w", err)
	}
	if repo == nil {
		return fmt.Errorf("failed to get repo. repo '%s' doesn't exist in db", repoDid)
	}

	// start a 1 min & go up to 1 hr between retries
	var retryCount = repo.RetryCount + 1
	var retryAfter = time.Now().Add(backoff(retryCount, 60) * 60).Unix()

	// remove null bytes
	errMsg = strings.ReplaceAll(errMsg, "\x00", "")

	repo.State = state
	repo.ErrorMsg = errMsg
	repo.RetryCount = retryCount
	repo.RetryAfter = retryAfter
	if err := db.UpsertRepo(ctx, r.db, repo); err != nil {
		return fmt.Errorf("failed to update repo state: %w", err)
	}
	return nil
}

func backoff(retries int, max int) time.Duration {
	dur := min(1<<retries, max)
	jitter := time.Millisecond * time.Duration(rand.Intn(1000))
	return time.Second*time.Duration(dur) + jitter
}

func (r *Resyncer) requestIndex(ctx context.Context, repoDid syntax.DID, branches []branch) error {
	r.logger.Info("requesting index", "repo", repoDid, "branches", branches)
	body, err := json.Marshal(map[string]any{
		"repo":     repoDid.String(),
		"branches": branches,
	})
	if err != nil {
		return fmt.Errorf("marshaling index request: %w", err)
	}

	endpoint := r.cfg.Search.ZoektUrl + "/admin/enqueueIndex"
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := r.httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("requesting zoekt index: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("non-ok status: %d", resp.StatusCode)
	}
	return nil
}
