package gitea

import (
	"context"
	"fmt"
	"path/filepath"

	"github.com/go-git/go-git/v5/plumbing/object"
)

func GetEntry(ctx context.Context, repoPath, ref, path string) (*object.TreeEntry, error) {
	if ref == "" {
		ref = "HEAD"
	}

	head, err := GetCommit(ctx, repoPath, ref)
	if err != nil {
		return nil, fmt.Errorf("get head commit: %w", err)
	}

	return GetEntryFromCommit(ctx, repoPath, head, path)
}

func GetEntryFromCommit(ctx context.Context, repoPath string, commit *object.Commit, path string) (*object.TreeEntry, error) {
	treePath := filepath.Dir(path)
	name := filepath.Base(path)

	// find subTree
	subRev := commit.Hash.String() + "^{tree}"
	if treePath != "." {
		subRev = commit.Hash.String() + ":" + treePath
	}
	subTree, err := GetTree(ctx, repoPath, subRev)
	if err != nil {
		return nil, fmt.Errorf("get subtree %s: %w", subRev, err)
	}

	// find entry
	entry, err := func(subTree *object.Tree) (*object.TreeEntry, error) {
		for _, entry := range subTree.Entries {
			if entry.Name == name {
				return &entry, nil
			}
		}
		return nil, fmt.Errorf("object doesn't exist")
	}(subTree)
	if err != nil {
		return nil, fmt.Errorf("get file: %w", err)
	}

	return entry, nil
}
