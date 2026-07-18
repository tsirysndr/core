package artifactstore

import (
	"bytes"
	"context"
	"io"
	"os"
	"strings"
	"sync"
	"testing"

	"github.com/aws/aws-sdk-go-v2/service/s3"
)

type mockS3Client struct {
	mu    sync.Mutex
	store map[string][]byte
}

func newMockS3Client() *mockS3Client {
	return &mockS3Client{
		store: make(map[string][]byte),
	}
}

func (m *mockS3Client) PutObject(ctx context.Context, params *s3.PutObjectInput, optFns ...func(*s3.Options)) (*s3.PutObjectOutput, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	b, err := io.ReadAll(params.Body)
	if err != nil {
		return nil, err
	}
	key := *params.Bucket + "/" + *params.Key
	m.store[key] = b
	return &s3.PutObjectOutput{}, nil
}

func (m *mockS3Client) GetObject(ctx context.Context, params *s3.GetObjectInput, optFns ...func(*s3.Options)) (*s3.GetObjectOutput, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	key := *params.Bucket + "/" + *params.Key
	data, ok := m.store[key]
	if !ok {
		return nil, os.ErrNotExist
	}
	return &s3.GetObjectOutput{
		Body: io.NopCloser(bytes.NewReader(data)),
	}, nil
}

func TestDiskStore(t *testing.T) {
	tempDir := t.TempDir()
	store, err := NewDiskStore(tempDir)
	if err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	ref := "logs/test.log"
	content := "hello world log content"

	if err := store.Put(ctx, ref, strings.NewReader(content)); err != nil {
		t.Fatalf("Put failed: %v", err)
	}

	rc, err := store.Open(ctx, ref)
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer rc.Close()

	got, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("ReadAll failed: %v", err)
	}
	if string(got) != content {
		t.Fatalf("got content %q, want %q", string(got), content)
	}
}

func TestDiskStoreTraversalProtection(t *testing.T) {
	tempDir := t.TempDir()
	store, err := NewDiskStore(tempDir)
	if err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	badRef := "../outside"

	err = store.Put(ctx, badRef, strings.NewReader("bad"))
	if err == nil {
		t.Fatal("expected error putting file outside diskDir, got nil")
	}

	_, err = store.Open(ctx, badRef)
	if err == nil {
		t.Fatal("expected error opening file outside diskDir, got nil")
	}
}

func TestS3Store(t *testing.T) {
	mock := newMockS3Client()
	store, err := NewS3Store(mock, "mybucket")
	if err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	ref := "logs/run1.log"
	content := "s3 log payload"

	if err := store.Put(ctx, ref, strings.NewReader(content)); err != nil {
		t.Fatalf("Put to S3 failed: %v", err)
	}

	rc, err := store.Open(ctx, ref)
	if err != nil {
		t.Fatalf("Open from S3 failed: %v", err)
	}
	defer rc.Close()

	got, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("ReadAll failed: %v", err)
	}
	if string(got) != content {
		t.Fatalf("got content %q, want %q", string(got), content)
	}
}
