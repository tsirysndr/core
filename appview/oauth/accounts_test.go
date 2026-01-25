package oauth

import (
	"testing"
)

func TestAccountRegistry_AddAccount(t *testing.T) {
	tests := []struct {
		name          string
		initial       []AccountInfo
		addDid        string
		addHandle     string
		addSessionId  string
		wantErr       error
		wantLen       int
		wantSessionId string
	}{
		{
			name:          "add first account",
			initial:       []AccountInfo{},
			addDid:        "did:plc:abc123",
			addHandle:     "alice.bsky.social",
			addSessionId:  "session-1",
			wantErr:       nil,
			wantLen:       1,
			wantSessionId: "session-1",
		},
		{
			name: "add second account",
			initial: []AccountInfo{
				{Did: "did:plc:abc123", SessionId: "session-1", AddedAt: 1000},
			},
			addDid:        "did:plc:def456",
			addHandle:     "bob.bsky.social",
			addSessionId:  "session-2",
			wantErr:       nil,
			wantLen:       2,
			wantSessionId: "session-2",
		},
		{
			name: "update existing account session",
			initial: []AccountInfo{
				{Did: "did:plc:abc123", SessionId: "old-session", AddedAt: 1000},
			},
			addDid:        "did:plc:abc123",
			addHandle:     "alice.bsky.social",
			addSessionId:  "new-session",
			wantErr:       nil,
			wantLen:       1,
			wantSessionId: "new-session",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			registry := &AccountRegistry{Accounts: tt.initial}
			err := registry.AddAccount(tt.addDid, tt.addHandle, tt.addSessionId)

			if err != tt.wantErr {
				t.Errorf("AddAccount() error = %v, want %v", err, tt.wantErr)
			}

			if len(registry.Accounts) != tt.wantLen {
				t.Errorf("AddAccount() len = %d, want %d", len(registry.Accounts), tt.wantLen)
			}

			found := registry.FindAccount(tt.addDid)
			if found == nil {
				t.Errorf("AddAccount() account not found after add")
				return
			}

			if found.SessionId != tt.wantSessionId {
				t.Errorf("AddAccount() sessionId = %s, want %s", found.SessionId, tt.wantSessionId)
			}
		})
	}
}

func TestAccountRegistry_AddAccount_MaxLimit(t *testing.T) {
	registry := &AccountRegistry{Accounts: make([]AccountInfo, 0, MaxAccounts)}

	for i := range MaxAccounts {
		err := registry.AddAccount("did:plc:user"+string(rune('a'+i)), "handle", "session")
		if err != nil {
			t.Fatalf("AddAccount() unexpected error on account %d: %v", i, err)
		}
	}

	if len(registry.Accounts) != MaxAccounts {
		t.Errorf("expected %d accounts, got %d", MaxAccounts, len(registry.Accounts))
	}

	err := registry.AddAccount("did:plc:overflow", "overflow", "session-overflow")
	if err != ErrMaxAccountsReached {
		t.Errorf("AddAccount() error = %v, want %v", err, ErrMaxAccountsReached)
	}

	if len(registry.Accounts) != MaxAccounts {
		t.Errorf("account added despite max limit, got %d", len(registry.Accounts))
	}
}

func TestAccountRegistry_RemoveAccount(t *testing.T) {
	tests := []struct {
		name      string
		initial   []AccountInfo
		removeDid string
		wantLen   int
		wantDids  []string
	}{
		{
			name: "remove existing account",
			initial: []AccountInfo{
				{Did: "did:plc:abc123", SessionId: "s1"},
				{Did: "did:plc:def456", SessionId: "s2"},
			},
			removeDid: "did:plc:abc123",
			wantLen:   1,
			wantDids:  []string{"did:plc:def456"},
		},
		{
			name: "remove non-existing account",
			initial: []AccountInfo{
				{Did: "did:plc:abc123", SessionId: "s1"},
			},
			removeDid: "did:plc:notfound",
			wantLen:   1,
			wantDids:  []string{"did:plc:abc123"},
		},
		{
			name: "remove last account",
			initial: []AccountInfo{
				{Did: "did:plc:abc123", SessionId: "s1"},
			},
			removeDid: "did:plc:abc123",
			wantLen:   0,
			wantDids:  []string{},
		},
		{
			name:      "remove from empty registry",
			initial:   []AccountInfo{},
			removeDid: "did:plc:abc123",
			wantLen:   0,
			wantDids:  []string{},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			registry := &AccountRegistry{Accounts: tt.initial}
			registry.RemoveAccount(tt.removeDid)

			if len(registry.Accounts) != tt.wantLen {
				t.Errorf("RemoveAccount() len = %d, want %d", len(registry.Accounts), tt.wantLen)
			}

			for _, wantDid := range tt.wantDids {
				if registry.FindAccount(wantDid) == nil {
					t.Errorf("RemoveAccount() expected %s to remain", wantDid)
				}
			}

			if registry.FindAccount(tt.removeDid) != nil && tt.wantLen < len(tt.initial) {
				t.Errorf("RemoveAccount() %s should have been removed", tt.removeDid)
			}
		})
	}
}

func TestAccountRegistry_FindAccount(t *testing.T) {
	registry := &AccountRegistry{
		Accounts: []AccountInfo{
			{Did: "did:plc:first", SessionId: "s1", AddedAt: 1000},
			{Did: "did:plc:second", SessionId: "s2", AddedAt: 2000},
			{Did: "did:plc:third", SessionId: "s3", AddedAt: 3000},
		},
	}

	t.Run("find existing account", func(t *testing.T) {
		found := registry.FindAccount("did:plc:second")
		if found == nil {
			t.Fatal("FindAccount() returned nil for existing account")
		}
		if found.SessionId != "s2" {
			t.Errorf("FindAccount() sessionId = %s, want s2", found.SessionId)
		}
	})

	t.Run("find non-existing account", func(t *testing.T) {
		found := registry.FindAccount("did:plc:notfound")
		if found != nil {
			t.Errorf("FindAccount() = %v, want nil", found)
		}
	})

	t.Run("returned pointer is mutable", func(t *testing.T) {
		found := registry.FindAccount("did:plc:first")
		if found == nil {
			t.Fatal("FindAccount() returned nil")
		}
		found.SessionId = "modified"

		refetch := registry.FindAccount("did:plc:first")
		if refetch.SessionId != "modified" {
			t.Errorf("FindAccount() pointer not referencing original, got %s", refetch.SessionId)
		}
	})
}
