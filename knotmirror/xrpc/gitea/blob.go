// Copyright 2021 The Gitea Authors. All rights reserved.
// SPDX-License-Identifier: MIT

package gitea

import (
	"bufio"
	"bytes"
	"context"
	"io"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/samber/lo"
)

func GetBlobSize(ctx context.Context, repoPath string, hash plumbing.Hash) (int64, error) {
	wr, rd, cancel := CatFileBatchCheck(ctx, repoPath)
	defer cancel()
	if _, err := wr.Write([]byte(hash.String() + "\n")); err != nil {
		return 0, err
	}
	_, _, size, err := ReadBatchLine(rd)
	return size, err
}

func EntrySizes(ctx context.Context, repoPath string, entries []object.TreeEntry) ([]int64, error) {
	sizes := make([]int64, len(entries))
	blobs := lo.Filter(lo.Range(len(entries)), func(i int, _ int) bool {
		return isBlobMode(entries[i].Mode)
	})
	if len(blobs) == 0 {
		return sizes, nil
	}
	wr, rd, cancel := CatFileBatchCheck(ctx, repoPath)
	defer cancel()
	for _, i := range blobs {
		if _, err := wr.Write([]byte(entries[i].Hash.String() + "\n")); err != nil {
			return sizes, err
		}
		_, typ, size, err := ReadBatchLine(rd)
		if err != nil {
			return sizes, err
		}
		if typ == "blob" {
			sizes[i] = size
		}
	}
	return sizes, nil
}

func isBlobMode(mode filemode.FileMode) bool {
	return mode == filemode.Regular || mode == filemode.Executable || mode == filemode.Symlink
}

// ReadBlob returns blob size and [io.ReadCloser] of that blob.
func ReadBlob(ctx context.Context, repoPath string, hash plumbing.Hash) (int64, io.ReadCloser, error) {
	wr, rd, cancel := CatFileBatch(ctx, repoPath)

	_, err := wr.Write([]byte(hash.String() + "\n"))
	if err != nil {
		cancel()
		return 0, nil, err
	}
	_, _, size, err := ReadBatchLine(rd)
	if err != nil {
		cancel()
		return 0, nil, err
	}

	if size < 4096 {
		bs, err := io.ReadAll(io.LimitReader(rd, size))
		defer cancel()
		if err != nil {
			return 0, nil, err
		}
		_, err = rd.Discard(1)
		return size, io.NopCloser(bytes.NewReader(bs)), err
	}

	return size, &blobReader{
		rd:     rd,
		n:      size,
		cancel: cancel,
	}, nil
}

type blobReader struct {
	rd     *bufio.Reader
	n      int64
	cancel func()
}

func (b *blobReader) Read(p []byte) (n int, err error) {
	if b.n <= 0 {
		return 0, io.EOF
	}
	if int64(len(p)) > b.n {
		p = p[0:b.n]
	}
	n, err = b.rd.Read(p)
	b.n -= int64(n)
	return n, err
}

// Close implements io.Closer
func (b *blobReader) Close() error {
	if b.rd == nil {
		return nil
	}

	defer b.cancel()

	if err := DiscardFull(b.rd, b.n+1); err != nil {
		return err
	}

	b.rd = nil

	return nil
}
