package db

import (
	"testing"

	"tangled.org/core/orm"
)

func seedFollow(t *testing.T, d *DB, userDid, subjectDid, rkey, followedAt string) {
	t.Helper()
	if _, err := d.Exec(
		`insert into follows (did, subject_did, rkey, created) values (?, ?, ?, ?)`,
		userDid, subjectDid, rkey, followedAt,
	); err != nil {
		t.Fatalf("seedFollow %s -> %s: %v", userDid, subjectDid, err)
	}
}

func seedStar(t *testing.T, d *DB, did, rkey, subject, created string) {
	t.Helper()
	if _, err := d.Exec(
		`insert into stars (did, rkey, subject_type, subject, created) values (?, ?, 'repo', ?, ?)`,
		did, rkey, subject, created,
	); err != nil {
		t.Fatalf("seedStar %s -> %s: %v", did, subject, err)
	}
}

func TestFilterInSubquery(t *testing.T) {
	f := orm.FilterInSubquery("did", "select subject_did from follows where user_did = ?", "did:plc:viewer")
	if got, want := f.Condition(), "did in (select subject_did from follows where user_did = ?)"; got != want {
		t.Errorf("Condition() = %q, want %q", got, want)
	}
	if args := f.Arg(); len(args) != 1 || args[0] != "did:plc:viewer" {
		t.Errorf("Arg() = %v, want [did:plc:viewer]", args)
	}
}

func TestMakeTimeline_FollowingOnly(t *testing.T) {
	d := newTestDB(t)

	const (
		viewer   = "did:plc:viewer"
		followed = "did:plc:followed"
		stranger = "did:plc:stranger"
	)

	// viewer follows `followed` but not `stranger`
	seedFollow(t, d, viewer, followed, "rkey-viewer-followed", "2024-01-01T00:00:00Z")

	// both users create repos
	seedRepo(t, d, followed, "knot.example.com", "followed-repo", "rkey-fr", "did:plc:repo-followed")
	seedRepo(t, d, stranger, "knot.example.com", "stranger-repo", "rkey-sr", "did:plc:repo-stranger")

	// both users star a repo
	seedStar(t, d, followed, "rkey-fs", "did:plc:repo-stranger", "2024-02-01T00:00:00Z")
	seedStar(t, d, stranger, "rkey-ss", "did:plc:repo-followed", "2024-02-01T00:00:00Z")

	// both users follow someone else
	seedFollow(t, d, followed, stranger, "rkey-ff", "2024-03-01T00:00:00Z")
	seedFollow(t, d, stranger, viewer, "rkey-sf", "2024-03-01T00:00:00Z")

	groups, err := MakeTimeline(d, 50, viewer, true)
	if err != nil {
		t.Fatalf("MakeTimeline(following): %v", err)
	}

	var nRepos, nStars, nFollows int
	for _, g := range groups {
		switch {
		case g.Primary.Repo != nil:
			nRepos++
			if g.Primary.Repo.Did != followed {
				t.Errorf("repo event from %q, want only %q", g.Primary.Repo.Did, followed)
			}
		case g.Primary.RepoStar != nil:
			nStars++
			if g.Primary.RepoStar.Star.Did != followed {
				t.Errorf("star event from %q, want only %q", g.Primary.RepoStar.Star.Did, followed)
			}
		case g.Primary.Follow != nil:
			nFollows++
			if g.Primary.Follow.UserDid != followed {
				t.Errorf("follow event from %q, want only %q", g.Primary.Follow.UserDid, followed)
			}
		}
	}

	if nRepos != 1 || nStars != 1 || nFollows != 1 {
		t.Errorf("got %d repo, %d star, %d follow events; want 1 of each", nRepos, nStars, nFollows)
	}
}

func TestMakeTimeline_FollowingNobody(t *testing.T) {
	d := newTestDB(t)

	// other users are active, but viewer follows nobody
	seedRepo(t, d, "did:plc:stranger", "knot.example.com", "repo", "rkey-r", "did:plc:repo-1")
	seedStar(t, d, "did:plc:stranger", "rkey-s", "did:plc:repo-1", "2024-02-01T00:00:00Z")
	seedFollow(t, d, "did:plc:stranger", "did:plc:other", "rkey-f", "2024-03-01T00:00:00Z")

	groups, err := MakeTimeline(d, 50, "did:plc:viewer", true)
	if err != nil {
		t.Fatalf("MakeTimeline(following nobody): %v", err)
	}
	if len(groups) != 0 {
		t.Errorf("expected empty following timeline, got %d groups", len(groups))
	}
}

func TestMakeTimeline_Global(t *testing.T) {
	d := newTestDB(t)

	seedRepo(t, d, "did:plc:a", "knot.example.com", "repo-a", "rkey-a", "did:plc:repo-a")
	seedStar(t, d, "did:plc:b", "rkey-bs", "did:plc:repo-a", "2024-02-01T00:00:00Z")
	seedFollow(t, d, "did:plc:b", "did:plc:a", "rkey-bf", "2024-03-01T00:00:00Z")

	groups, err := MakeTimeline(d, 50, "", false)
	if err != nil {
		t.Fatalf("MakeTimeline(global): %v", err)
	}
	if len(groups) != 3 {
		t.Errorf("expected 3 groups in global timeline, got %d", len(groups))
	}
}
