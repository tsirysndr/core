package webhook

import (
	"context"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
)

func TestPullStateEvent(t *testing.T) {
	tests := []struct {
		name       string
		state      models.PullState
		wantEvent  models.WebhookEvent
		wantAction string
		wantOk     bool
	}{
		{"merged", models.PullMerged, models.WebhookEventPullRequestMerged, "merged", true},
		{"closed", models.PullClosed, models.WebhookEventPullRequestClosed, "closed", true},
		{"reopened", models.PullOpen, models.WebhookEventPullRequestReopened, "reopened", true},
		{"abandoned", models.PullAbandoned, "", "", false},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			event, action, ok := pullStateEvent(tt.state)
			if event != tt.wantEvent || action != tt.wantAction || ok != tt.wantOk {
				t.Errorf("pullStateEvent(%v) = (%q, %q, %v), want (%q, %q, %v)",
					tt.state, event, action, ok, tt.wantEvent, tt.wantAction, tt.wantOk)
			}
		})
	}
}

func TestBuildPullRequestPayload(t *testing.T) {
	const baseUrl = "https://tangled.org"

	targetDid := syntax.DID("did:plc:target")
	forkDid := syntax.DID("did:plc:fork")

	repo := &models.Repo{
		Did:     "did:plc:target",
		Name:    "some-repo",
		Knot:    "knot.example.com",
		Rkey:    "some-repo",
		Created: time.Date(2025, 9, 15, 8, 57, 23, 0, time.UTC),
	}

	basePull := func() models.Pull {
		return models.Pull{
			PullId:       4,
			RepoDid:      targetDid,
			OwnerDid:     "did:plc:author",
			Title:        "add dark mode",
			Body:         "implements dark mode",
			TargetBranch: "main",
			State:        models.PullOpen,
			Created:      time.Date(2025, 9, 16, 10, 0, 0, 0, time.UTC),
			Submissions: []*models.PullSubmission{
				{RoundNumber: 0, SourceRev: "aaaa000"},
				{RoundNumber: 1, SourceRev: "bbbb111"},
			},
		}
	}

	t.Run("patch based", func(t *testing.T) {
		pull := basePull()
		pull.PullSource = nil

		payload := buildPullRequestPayload("created", repo, &pull, "did:plc:author", baseUrl)

		if payload.Action != "created" {
			t.Errorf("action = %q, want %q", payload.Action, "created")
		}
		pr := payload.PullRequest
		if pr.Number != 4 {
			t.Errorf("number = %d, want 4", pr.Number)
		}
		if pr.State != "open" {
			t.Errorf("state = %q, want %q", pr.State, "open")
		}
		if pr.Source != nil {
			t.Errorf("source = %+v, want nil for patch-based pull", pr.Source)
		}
		if pr.RoundNumber != 1 {
			t.Errorf("round_number = %d, want 1", pr.RoundNumber)
		}
		wantHtmlUrl := "https://tangled.org/did:plc:target/some-repo/pulls/4"
		if pr.HtmlUrl != wantHtmlUrl {
			t.Errorf("html_url = %q, want %q", pr.HtmlUrl, wantHtmlUrl)
		}
		wantPatchUrl := wantHtmlUrl + "/round/1.patch"
		if pr.PatchUrl != wantPatchUrl {
			t.Errorf("patch_url = %q, want %q", pr.PatchUrl, wantPatchUrl)
		}
		if pr.Owner.Did != "did:plc:author" {
			t.Errorf("owner.did = %q, want %q", pr.Owner.Did, "did:plc:author")
		}
		if payload.Sender.Did != "did:plc:author" {
			t.Errorf("sender.did = %q, want %q", payload.Sender.Did, "did:plc:author")
		}
		if payload.Repository.FullName != "did:plc:target/some-repo" {
			t.Errorf("repository.full_name = %q, want %q", payload.Repository.FullName, "did:plc:target/some-repo")
		}
	})

	t.Run("branch based", func(t *testing.T) {
		pull := basePull()
		pull.PullSource = &models.PullSource{
			Branch:  "dark-mode",
			RepoDid: &targetDid,
		}

		payload := buildPullRequestPayload("merged", repo, &pull, "did:plc:merger", baseUrl)

		pr := payload.PullRequest
		if pr.Source == nil {
			t.Fatal("source = nil, want non-nil for branch-based pull")
		}
		if pr.Source.Branch != "dark-mode" {
			t.Errorf("source.branch = %q, want %q", pr.Source.Branch, "dark-mode")
		}
		if pr.Source.Repo != "" {
			t.Errorf("source.repo = %q, want empty for branch-based pull", pr.Source.Repo)
		}
		if pr.Source.Sha != "bbbb111" {
			t.Errorf("source.sha = %q, want %q", pr.Source.Sha, "bbbb111")
		}
		if payload.Sender.Did != "did:plc:merger" {
			t.Errorf("sender.did = %q, want %q", payload.Sender.Did, "did:plc:merger")
		}
	})

	t.Run("fork based", func(t *testing.T) {
		pull := basePull()
		pull.PullSource = &models.PullSource{
			Branch:  "dark-mode",
			RepoDid: &forkDid,
		}

		payload := buildPullRequestPayload("created", repo, &pull, "did:plc:author", baseUrl)

		pr := payload.PullRequest
		if pr.Source == nil {
			t.Fatal("source = nil, want non-nil for fork-based pull")
		}
		if pr.Source.Repo != "did:plc:fork" {
			t.Errorf("source.repo = %q, want %q", pr.Source.Repo, "did:plc:fork")
		}
	})

	t.Run("no submissions", func(t *testing.T) {
		pull := basePull()
		pull.Submissions = nil

		payload := buildPullRequestPayload("created", repo, &pull, "did:plc:author", baseUrl)

		pr := payload.PullRequest
		if pr.RoundNumber != 0 {
			t.Errorf("round_number = %d, want 0", pr.RoundNumber)
		}
		if pr.PatchUrl != "" {
			t.Errorf("patch_url = %q, want empty when there are no submissions", pr.PatchUrl)
		}
	})
}

type notifierTestEnv struct {
	notifier *Notifier
	webhook  *models.Webhook
	db       *db.DB
	received chan string
}

// newNotifierTestEnv sets up a real sqlite db with a repo and a webhook
// subscribed to the given events, delivering to a local test server
func newNotifierTestEnv(t *testing.T, events []string) *notifierTestEnv {
	t.Helper()

	d, err := db.Make(context.Background(), filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("Make: %v", err)
	}
	t.Cleanup(func() { d.Close() })

	tx, err := d.Begin()
	if err != nil {
		t.Fatalf("Begin: %v", err)
	}
	if err := db.AddRepo(tx, &models.Repo{
		Did:     "did:plc:owner",
		Name:    "some-repo",
		Knot:    "knot.example.com",
		Rkey:    "some-repo",
		RepoDid: "did:plc:repo1",
	}); err != nil {
		t.Fatalf("AddRepo: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	received := make(chan string, 1)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		received <- r.Header.Get("X-Tangled-Event")
		w.WriteHeader(http.StatusOK)
	}))
	t.Cleanup(srv.Close)

	webhook := &models.Webhook{
		RepoDid: syntax.DID("did:plc:repo1"),
		Url:     srv.URL,
		Active:  true,
		Events:  events,
	}
	if err := db.AddWebhook(d, webhook); err != nil {
		t.Fatalf("AddWebhook: %v", err)
	}

	return &notifierTestEnv{
		notifier: NewNotifier(d, "https://tangled.org", true),
		webhook:  webhook,
		db:       d,
		received: received,
	}
}

func (env *notifierTestEnv) awaitDelivery(t *testing.T, wantEvent string) {
	t.Helper()

	select {
	case event := <-env.received:
		if event != wantEvent {
			t.Errorf("X-Tangled-Event = %q, want %q", event, wantEvent)
		}
	case <-time.After(10 * time.Second):
		t.Fatalf("webhook %s not delivered", wantEvent)
	}

	// wait for the delivery record so the sender goroutine finishes
	// before the db is closed
	deadline := time.Now().Add(10 * time.Second)
	for {
		deliveries, err := db.GetWebhookDeliveries(env.db, env.webhook.Id, 10)
		if err == nil && len(deliveries) > 0 {
			if !deliveries[0].Success {
				t.Errorf("delivery recorded as failed, want success")
			}
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("delivery record not written")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func testPull(state models.PullState) *models.Pull {
	return &models.Pull{
		PullId:       1,
		RepoDid:      syntax.DID("did:plc:repo1"),
		OwnerDid:     "did:plc:author",
		Title:        "hello",
		TargetBranch: "main",
		State:        state,
		Created:      time.Now(),
	}
}

// Pull request events fire from http handlers, whose request context is
// canceled as soon as the handler returns. Deliveries run in background
// goroutines and must not be cut short by that cancellation.
func TestPullRequestEventDeliversAfterContextCancel(t *testing.T) {
	env := newNotifierTestEnv(t, []string{string(models.WebhookEventPullRequestCreated)})

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	env.notifier.NewPull(ctx, testPull(models.PullOpen))
	env.awaitDelivery(t, "pull_request:created")
}

func TestNotifierDeliversPullRequestEvents(t *testing.T) {
	allEvents := []string{
		string(models.WebhookEventPullRequestCreated),
		string(models.WebhookEventPullRequestResubmitted),
		string(models.WebhookEventPullRequestMerged),
		string(models.WebhookEventPullRequestClosed),
		string(models.WebhookEventPullRequestReopened),
	}
	actor := syntax.DID("did:plc:actor")

	tests := []struct {
		name      string
		notify    func(*Notifier, context.Context)
		wantEvent string
	}{
		{
			"resubmitted",
			func(n *Notifier, ctx context.Context) { n.ResubmitPull(ctx, testPull(models.PullOpen)) },
			"pull_request:resubmitted",
		},
		{
			"merged",
			func(n *Notifier, ctx context.Context) { n.NewPullState(ctx, actor, testPull(models.PullMerged)) },
			"pull_request:merged",
		},
		{
			"closed",
			func(n *Notifier, ctx context.Context) { n.NewPullState(ctx, actor, testPull(models.PullClosed)) },
			"pull_request:closed",
		},
		{
			"reopened",
			func(n *Notifier, ctx context.Context) { n.NewPullState(ctx, actor, testPull(models.PullOpen)) },
			"pull_request:reopened",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			env := newNotifierTestEnv(t, allEvents)
			tt.notify(env.notifier, context.Background())
			env.awaitDelivery(t, tt.wantEvent)
		})
	}
}
