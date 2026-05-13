package repoinfo

import "testing"

func TestSlug(t *testing.T) {
	cases := []struct {
		name string
		info RepoInfo
		want string
	}{
		{"name preferred over rkey", RepoInfo{Name: "barnacle", Rkey: "3kabc"}, "barnacle"},
		{"name equals rkey", RepoInfo{Name: "clam", Rkey: "clam"}, "clam"},
		{"name empty falls to rkey", RepoInfo{Rkey: "limpet"}, "limpet"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := c.info.Slug(); got != c.want {
				t.Errorf("Slug() = %q, want %q", got, c.want)
			}
		})
	}
}

func TestFullName_PrefersNameOverRkey(t *testing.T) {
	info := RepoInfo{OwnerHandle: "boltless.dev", Name: "uni", Rkey: "3kabcxyz"}
	if got, want := info.FullName(), "boltless.dev/uni"; got != want {
		t.Errorf("FullName() = %q, want %q", got, want)
	}
	if got, want := info.FullNameWithoutAt(), "boltless.dev/uni"; got != want {
		t.Errorf("FullNameWithoutAt() = %q, want %q", got, want)
	}
}

func TestFullName_FallsBackToRkey(t *testing.T) {
	info := RepoInfo{OwnerHandle: "akshay.dev", Rkey: "3kabcxyz"}
	if got, want := info.FullName(), "akshay.dev/3kabcxyz"; got != want {
		t.Errorf("FullName() = %q, want %q", got, want)
	}
}

func TestFullNameWithoutAt_FlattensDid(t *testing.T) {
	info := RepoInfo{OwnerDid: "did:plc:boltless", Name: "nautilus", Rkey: "nautilus"}
	if got, want := info.FullNameWithoutAt(), "did-plc-boltless/nautilus"; got != want {
		t.Errorf("FullNameWithoutAt() = %q, want %q", got, want)
	}
}
