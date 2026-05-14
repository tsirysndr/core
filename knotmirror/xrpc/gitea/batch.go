// NOTE: lot's of code compied from Gitea with slight modification to use go-git objects

package gitea

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"io"
	"math"
	"os/exec"
	"strconv"
	"strings"

	"github.com/djherbis/buffer"
	"github.com/djherbis/nio/v3"
	"github.com/go-git/go-git/v5/plumbing"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"github.com/go-git/go-git/v5/plumbing/hash"
	"github.com/go-git/go-git/v5/plumbing/object"
)

func GetCommit(ctx context.Context, repoPath, rev string) (*object.Commit, error) {
	wr, rd, cancel := CatFileBatch(ctx, repoPath)
	defer cancel()

	if _, err := wr.Write([]byte(rev + "\n")); err != nil {
		return nil, fmt.Errorf("write rev: %w", err)
	}
	sha, typ, size, err := ReadBatchLine(rd)
	if err != nil {
		return nil, err
	}
	if typ != "commit" {
		if err := DiscardFull(rd, size+1); err != nil {
			return nil, err
		}
		return nil, fmt.Errorf("unexpected type: %s for commit: %s", typ, rev)
	}
	commit, err := ReadCommit(plumbing.NewHash(string(sha)), io.LimitReader(rd, size))
	if err != nil {
		return nil, fmt.Errorf("read commit %s: %w", rev, err)
	}
	if _, err := rd.Discard(1); err != nil {
		return nil, err
	}
	return commit, nil
}

func GetTree(ctx context.Context, repoPath, rev string) (*object.Tree, error) {
	wr, rd, cancel := CatFileBatch(ctx, repoPath)
	defer cancel()

	if _, err := wr.Write([]byte(rev + "\n")); err != nil {
		return nil, fmt.Errorf("write rev: %w", err)
	}
	sha, typ, size, err := ReadBatchLine(rd)
	if err != nil {
		return nil, fmt.Errorf("resolve %s: %w", rev, err)
	}
	if typ != "tree" {
		if err := DiscardFull(rd, size+1); err != nil {
			return nil, err
		}
		return nil, fmt.Errorf("unexpected type: %s for tree: %s", typ, rev)
	}

	entries, err := catBatchParseTreeEntries(rd, size)
	if err != nil {
		return nil, fmt.Errorf("read tree %s: %w", rev, err)
	}
	return &object.Tree{
		Hash:    plumbing.NewHash(string(sha)),
		Entries: entries,
	}, nil
}

func catBatchParseTreeEntries(rd *bufio.Reader, sz int64) ([]object.TreeEntry, error) {
	entries := make([]object.TreeEntry, 0, 10)
loop:
	for sz > 0 {
		mode, fname, sha, count, err := ParseCatFileTreeLine(rd)
		if err != nil {
			if err == io.EOF {
				break loop
			}
			return nil, err
		}
		modeNum, err := strconv.ParseUint(string(mode), 8, 32)
		if err != nil {
			return nil, err
		}
		sz -= int64(count)
		entry := object.TreeEntry{
			Name: string(fname),
			Mode: filemode.FileMode(modeNum),
			Hash: plumbing.Hash(sha),
		}
		entries = append(entries, entry)
	}
	if _, err := rd.Discard(1); err != nil {
		return entries, err
	}
	return entries, nil
}

func CatFileBatch(ctx context.Context, repoPath string) (io.WriteCloser, *bufio.Reader, func()) {
	batchStdinReader, batchStdinWriter := io.Pipe()
	batchStdoutReader, batchStdoutWriter := nio.Pipe(buffer.New(32 * 1024))
	ctx, ctxCancel := context.WithCancel(ctx)
	closed := make(chan struct{})
	cancel := func() {
		ctxCancel()
		_ = batchStdinWriter.Close()
		_ = batchStdoutReader.Close()
		<-closed
	}

	// Ensure cancel is called as soon as the provided context is cancelled
	go func() {
		<-ctx.Done()
		cancel()
	}()

	go func() {
		stderr := &strings.Builder{}
		cmd := exec.CommandContext(ctx, "git", "-C", repoPath, "cat-file", "--batch")
		cmd.Stdin = batchStdinReader
		cmd.Stdout = batchStdoutWriter
		cmd.Stderr = stderr
		if err := cmd.Run(); err != nil {
			_ = batchStdinReader.CloseWithError(fmt.Errorf("%w\n%s", err, stderr.String()))
			_ = batchStdoutWriter.CloseWithError(fmt.Errorf("%w\n%s", err, stderr.String()))
		} else {
			_ = batchStdoutWriter.Close()
			_ = batchStdinReader.Close()
		}
		close(closed)
	}()

	batchReader := bufio.NewReaderSize(batchStdoutReader, 32*1024)
	return batchStdinWriter, batchReader, cancel
}

func ReadBatchLine(reader io.Reader) (sha []byte, typ string, size int64, err error) {
	rd, ok := reader.(*bufio.Reader)
	if !ok {
		rd = bufio.NewReader(reader)
	}
	typ, err = rd.ReadString('\n')
	if err != nil {
		return sha, typ, size, err
	}
	if len(typ) == 1 {
		typ, err = rd.ReadString('\n')
		if err != nil {
			return sha, typ, size, err
		}
	}
	idx := strings.IndexByte(typ, ' ')
	if idx < 0 {
		return sha, typ, size, fmt.Errorf("missing sha: %s", sha)
	}
	sha = []byte(typ[:idx])
	typ = typ[idx+1:]

	idx = strings.IndexByte(typ, ' ')
	if idx < 0 {
		return sha, typ, size, fmt.Errorf("missing size: %s", sha)
	}

	sizeStr := typ[idx+1 : len(typ)-1]
	typ = typ[:idx]

	size, err = strconv.ParseInt(sizeStr, 10, 64)
	return sha, typ, size, err
}

// NOTE: readCommit doesn't return complete go-git [object.Commit] object!
// The embedded object store is missing, so calling method from returned commit
// can lead to panic.
func ReadCommit(oid plumbing.Hash, reader io.Reader) (*object.Commit, error) {
	commit := &object.Commit{
		Hash: oid,
	}

	payloadSB := new(strings.Builder)
	signatureSB := new(strings.Builder)
	messageSB := new(strings.Builder)
	firstLine := true
	message := false
	pgpsig := false

	bufReader, ok := reader.(*bufio.Reader)
	if !ok {
		bufReader = bufio.NewReader(reader)
	}

readLoop:
	for {
		line, err := bufReader.ReadBytes('\n')
		if err != nil {
			if err == io.EOF {
				if message {
					_, _ = messageSB.Write(line)
				}
				_, _ = payloadSB.Write(line)
				break readLoop
			}
			return nil, err
		}
		if pgpsig {
			if len(line) > 0 && line[0] == ' ' {
				_, _ = signatureSB.Write(line[1:])
				continue
			}
			pgpsig = false
		}

		if !message {
			// This is probably not correct but is copied from go-gits interpretation...
			trimmed := bytes.TrimSpace(line)
			if len(trimmed) == 0 {
				message = true
				_, _ = payloadSB.Write(line)
				continue
			}

			split := bytes.SplitN(trimmed, []byte{' '}, 2)
			var data []byte
			if len(split) > 1 {
				data = split[1]
			}

			switch string(split[0]) {
			case "tree":
				commit.TreeHash = plumbing.NewHash(string(data))
				_, _ = payloadSB.Write(line)
			case "parent":
				commit.ParentHashes = append(commit.ParentHashes, plumbing.NewHash(string(data)))
				_, _ = payloadSB.Write(line)
			case "author":
				commit.Author.Decode(data)
				_, _ = payloadSB.Write(line)
			case "committer":
				commit.Committer.Decode(data)
				_, _ = payloadSB.Write(line)
			case "gpgsig":
				fallthrough
			case "gpgsig-sha256": // FIXME: no intertop, so only 1 exists at present.
				_, _ = signatureSB.Write(data)
				_ = signatureSB.WriteByte('\n')
				pgpsig = true
			default:
				// If the first line is not any of the known headers, then it is probably the prefix added when git cat-file is called with --batch, and that is not part of the payload
				if !firstLine {
					// Every subsequent header field is added to the payload
					_, _ = payloadSB.Write(line)
				}
			}
		} else {
			_, _ = messageSB.Write(line)
			_, _ = payloadSB.Write(line)
		}

		firstLine = false
	}
	commit.Message = messageSB.String()
	commit.PGPSignature = signatureSB.String()

	return commit, nil
}

// ParseCatFileTreeLine reads an entry from a tree in a cat-file --batch stream
// This carefully avoids allocations - except where fnameBuf is too small.
// It is recommended therefore to pass in an fnameBuf large enough to avoid almost all allocations
//
// Each line is composed of:
// <mode-in-ascii-dropping-initial-zeros> SP <fname> NUL <binary HASH>
//
// We don't attempt to convert the raw HASH to save a lot of time
func ParseCatFileTreeLine(rd *bufio.Reader) (mode, fname, sha []byte, n int, err error) {
	modeBuf := make([]byte, 40)
	fnameBuf := make([]byte, 4096)
	shaBuf := make([]byte, hash.HexSize)

	var readBytes []byte

	// Read the Mode & fname
	readBytes, err = rd.ReadSlice('\x00')
	if err != nil {
		return mode, fname, sha, n, err
	}
	idx := bytes.IndexByte(readBytes, ' ')
	if idx < 0 {
		return mode, fname, sha, n, fmt.Errorf("missing")
	}

	n += idx + 1
	copy(modeBuf, readBytes[:idx])
	if len(modeBuf) >= idx {
		modeBuf = modeBuf[:idx]
	} else {
		modeBuf = append(modeBuf, readBytes[len(modeBuf):idx]...)
	}
	mode = modeBuf

	readBytes = readBytes[idx+1:]

	// Deal with the fname
	copy(fnameBuf, readBytes)
	if len(fnameBuf) > len(readBytes) {
		fnameBuf = fnameBuf[:len(readBytes)]
	} else {
		fnameBuf = append(fnameBuf, readBytes[len(fnameBuf):]...)
	}
	for err == bufio.ErrBufferFull {
		readBytes, err = rd.ReadSlice('\x00')
		fnameBuf = append(fnameBuf, readBytes...)
	}
	n += len(fnameBuf)
	if err != nil {
		return mode, fname, sha, n, err
	}
	fnameBuf = fnameBuf[:len(fnameBuf)-1]
	fname = fnameBuf

	// Deal with the binary hash
	idx = 0
	length := hash.HexSize / 2
	for idx < length {
		var read int
		read, err = rd.Read(shaBuf[idx:length])
		n += read
		if err != nil {
			return mode, fname, sha, n, err
		}
		idx += read
	}
	sha = shaBuf
	return mode, fname, sha, n, err
}

func DiscardFull(rd *bufio.Reader, discard int64) error {
	if discard > math.MaxInt32 {
		n, err := rd.Discard(math.MaxInt32)
		discard -= int64(n)
		if err != nil {
			return err
		}
	}
	for discard > 0 {
		n, err := rd.Discard(int(discard))
		discard -= int64(n)
		if err != nil {
			return err
		}
	}
	return nil
}
