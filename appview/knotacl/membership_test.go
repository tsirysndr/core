package knotacl

import (
	"context"
	"slices"
	"testing"

	"tangled.org/core/appview/db"
	"tangled.org/core/orm"
)

func registerOwner(t *testing.T, d *db.DB, host, ownerDid string) {
	t.Helper()
	if err := db.AddKnot(d, host, ownerDid); err != nil {
		t.Fatalf("AddKnot: %v", err)
	}
	if err := db.MarkRegistered(d, orm.FilterEq("domain", host), orm.FilterEq("did", ownerDid)); err != nil {
		t.Fatalf("MarkRegistered: %v", err)
	}
}

func TestService_IsKnotMemberNative(t *testing.T) {
	ctx := context.Background()
	svc, d, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, members: []string{testCollab}}, nil)

	if !svc.IsKnotMember(ctx, host, testCollab) {
		t.Error("a did in the native listMembers must read as a member")
	}
	if svc.IsKnotMember(ctx, host, testStrange) {
		t.Error("a stranger must not read as a member")
	}

	registerOwner(t, d, host, testOwner)
	if !svc.IsKnotMember(ctx, host, testOwner) {
		t.Error("a registered owner must read as a member of its own native knot")
	}
}

func TestService_IsKnotMemberLegacy(t *testing.T) {
	ctx := context.Background()
	svc, _, host := newServiceEnv(t, &fakeKnot{version: "v1.14.0"}, nil)

	if !svc.IsKnotMember(ctx, host, testOwner) {
		t.Error("the casbin-seeded owner must read as a member on an old knot")
	}
	if svc.IsKnotMember(ctx, host, testStrange) {
		t.Error("a stranger must not read as a member on an old knot")
	}
}

func TestService_KnotsForUserNativeMemberAndOwner(t *testing.T) {
	ctx := context.Background()
	svc, d, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, members: []string{testCollab}}, nil)
	registerOwner(t, d, host, testOwner)

	if got := svc.KnotsForUser(ctx, testCollab); !slices.Contains(got, host) {
		t.Errorf("KnotsForUser(member) = %v; a native member must surface via the listMembers fan-out, not casbin", got)
	}
	if got := svc.KnotsForUser(ctx, testOwner); !slices.Contains(got, host) {
		t.Errorf("KnotsForUser(owner) = %v, want the registered knot", got)
	}
	if got := svc.KnotsForUser(ctx, testStrange); slices.Contains(got, host) {
		t.Errorf("KnotsForUser(stranger) = %v, want the knot omitted", got)
	}
}

func TestService_KnotsForUserNativeListDownDegrades(t *testing.T) {
	ctx := context.Background()
	svc, d, host := newServiceEnv(t, &fakeKnot{version: "v1.15.0", capabilities: capsKnotACL, listStatus: 500, members: []string{testCollab}}, nil)
	registerOwner(t, d, host, testOwner)

	if got := svc.KnotsForUser(ctx, testOwner); !slices.Contains(got, host) {
		t.Errorf("KnotsForUser(owner) = %v; an owner must surface from registrations even when the knot list is down", got)
	}
	if got := svc.KnotsForUser(ctx, testCollab); slices.Contains(got, host) {
		t.Errorf("KnotsForUser(member) = %v; a member must not surface when the bounded fan-out cannot reach the knot", got)
	}
}
