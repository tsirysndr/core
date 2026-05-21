package cursor

import (
	"sync"
)

type MemoryStore struct {
	store sync.Map
}

func (m *MemoryStore) Set(key string, cursor int64) {
	m.store.Store(key, cursor)
}

func (m *MemoryStore) Get(key string) (cursor int64) {
	if result, ok := m.store.Load(key); ok {
		if val, ok := result.(int64); ok {
			return val
		}
	}
	return 0
}
