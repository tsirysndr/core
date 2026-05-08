package compat113

import (
	"encoding/json"
	"io"

	lexutil "github.com/bluesky-social/indigo/lex/util"
	"tangled.org/core/api/tangled"
)

func Collaborator(r *tangled.RepoCollaborator) *lexutil.LexiconTypeDecoder {
	return &lexutil.LexiconTypeDecoder{Val: &collaboratorWrapper{inner: r}}
}

func Pull(r *tangled.RepoPull) *lexutil.LexiconTypeDecoder {
	return &lexutil.LexiconTypeDecoder{Val: &pullWrapper{inner: r}}
}

type collaboratorWrapper struct {
	LexiconTypeID string `cborgen:"$type,const=sh.tangled.repo.collaborator"`
	inner         *tangled.RepoCollaborator
}

func (c *collaboratorWrapper) MarshalJSON() ([]byte, error) {
	c.inner.LexiconTypeID = "sh.tangled.repo.collaborator"
	return marshalWithRepoDidShadow(c.inner, false)
}

func (c *collaboratorWrapper) MarshalCBOR(w io.Writer) error {
	return c.inner.MarshalCBOR(w)
}

type pullWrapper struct {
	LexiconTypeID string `cborgen:"$type,const=sh.tangled.repo.pull"`
	inner         *tangled.RepoPull
}

func (c *pullWrapper) MarshalJSON() ([]byte, error) {
	c.inner.LexiconTypeID = "sh.tangled.repo.pull"
	return marshalWithRepoDidShadow(c.inner, true)
}

func (c *pullWrapper) MarshalCBOR(w io.Writer) error {
	return c.inner.MarshalCBOR(w)
}

func marshalWithRepoDidShadow(inner any, nestedTarget bool) ([]byte, error) {
	raw, err := json.Marshal(inner)
	if err != nil {
		return nil, err
	}
	var top map[string]json.RawMessage
	if err := json.Unmarshal(raw, &top); err != nil {
		return raw, nil
	}
	if nestedTarget {
		injectIntoNested(top, "target")
		injectIntoNested(top, "source")
	} else {
		addRepoDidShadow(top)
	}
	return json.Marshal(top)
}

func injectIntoNested(parent map[string]json.RawMessage, key string) {
	raw, ok := parent[key]
	if !ok {
		return
	}
	var nested map[string]json.RawMessage
	if err := json.Unmarshal(raw, &nested); err != nil {
		return
	}
	addRepoDidShadow(nested)
	if reb, err := json.Marshal(nested); err == nil {
		parent[key] = reb
	}
}

func addRepoDidShadow(m map[string]json.RawMessage) {
	if _, has := m["repoDid"]; has {
		return
	}
	if v, ok := m["repo"]; ok {
		m["repoDid"] = v
	}
}
