package validator

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/models"
	"tangled.org/core/consts"
	"tangled.org/core/rbac"
)

func unreachableListValidator(t *testing.T) (*Validator, string) {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case strings.HasSuffix(r.URL.Path, tangled.KnotVersionNSID):
			json.NewEncoder(w).Encode(tangled.KnotVersion_Output{Version: "v1.15.0", Capabilities: []string{string(consts.CapKnotACL)}})
		case strings.HasSuffix(r.URL.Path, tangled.RepoListCollaboratorsNSID):
			http.Error(w, "list down", http.StatusInternalServerError)
		default:
			http.NotFound(w, r)
		}
	}))
	t.Cleanup(srv.Close)
	host := strings.TrimPrefix(srv.URL, "http://")

	dir := t.TempDir()
	enforcer, err := rbac.NewEnforcer(filepath.Join(dir, "rbac.db"))
	if err != nil {
		t.Fatalf("NewEnforcer: %v", err)
	}
	d, err := db.Make(context.Background(), filepath.Join(dir, "appview.db"))
	if err != nil {
		t.Fatalf("db.Make: %v", err)
	}
	svc := knotacl.NewService(enforcer, d, true, slog.New(slog.NewTextHandler(io.Discard, nil)))
	return &Validator{acl: svc}, host
}

func TestValidateLabelOp_MalformedRejectedBeforePermCheck(t *testing.T) {
	v, host := unreachableListValidator(t)
	def := &models.LabelDefinition{Did: "did:plc:akshay", Rkey: "deadbeef"}
	repo := &models.Repo{Did: "did:plc:akshay", Knot: host, RepoDid: "did:plc:limpet"}

	op := &models.LabelOp{
		Did:        "did:plc:scallop",
		OperandKey: "does-not-match-the-def-aturi",
		Operation:  "garbage",
	}
	err := v.ValidateLabelOp(context.Background(), def, repo, op)
	if errors.Is(err, knotacl.ErrKnotUnreachable) {
		t.Fatalf("malformed op returned ErrKnotUnreachable; structural validation did not run before the perm check")
	}
	if err == nil || !strings.Contains(err.Error(), "operand key") {
		t.Fatalf("want a structural operand-key error, got %v", err)
	}
}

func TestValidateLabelOp_WellFormedFailsOpenWhenKnotUnreachable(t *testing.T) {
	v, host := unreachableListValidator(t)
	def := &models.LabelDefinition{Did: "did:plc:akshay", Rkey: "deadbeef"}
	repo := &models.Repo{Did: "did:plc:akshay", Knot: host, RepoDid: "did:plc:limpet"}

	op := &models.LabelOp{
		Did:         "did:plc:scallop",
		OperandKey:  def.AtUri().String(),
		Operation:   models.LabelOperationAdd,
		Subject:     "at://did:plc:limpet/sh.tangled.repo.issue/abc123",
		PerformedAt: time.Now(),
	}
	err := v.ValidateLabelOp(context.Background(), def, repo, op)
	if !errors.Is(err, knotacl.ErrKnotUnreachable) {
		t.Fatalf("well-formed op against an unreachable knot = %v, want ErrKnotUnreachable so the ingester fails open", err)
	}
}
