package gitea

import (
	"context"
	"errors"
	"io"

	"github.com/go-git/go-git/v5/config"
)

var (
	ErrMissingGitModules = errors.New("no .gitmodules file found")
	ErrInvalidGitModules = errors.New("invalid .gitmodules file")
)

func GetSubmodules(ctx context.Context, repoPath, ref string) (*config.Modules, error) {
	modulesEntry, err := GetEntry(ctx, repoPath, ref, ".gitmodules")
	if err != nil {
		return nil, ErrMissingGitModules
	}

	// at the moment we do not strictly limit the size of the .gitmodules file because some users would have huge .gitmodules files (>1MB)
	_, reader, err := ReadBlob(ctx, repoPath, modulesEntry.Hash)
	if err != nil {
		return nil, err
	}

	modulesContent, err := io.ReadAll(reader)
	if err != nil {
		return nil, err
	}

	modules := config.NewModules()
	if err := modules.Unmarshal(modulesContent); err != nil {
		return nil, ErrInvalidGitModules
	}

	return modules, nil
}
