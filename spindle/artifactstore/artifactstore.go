package artifactstore

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	spindleconfig "tangled.org/core/spindle/config"
)

type Writer interface {
	Put(ctx context.Context, ref string, r io.Reader) error
}

type Reader interface {
	Open(ctx context.Context, ref string) (io.ReadCloser, error)
}

type Store interface {
	Writer
	Reader
}

type DiskStore struct {
	root string
}

func NewDiskStore(root string) (*DiskStore, error) {
	if root == "" {
		return nil, fmt.Errorf("artifact disk directory is required")
	}
	return &DiskStore{root: filepath.Clean(root)}, nil
}

func (s *DiskStore) Put(_ context.Context, ref string, r io.Reader) error {
	path, err := s.resolve(ref)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return fmt.Errorf("mkdir for artifact %q: %w", path, err)
	}
	tmpFile, err := os.CreateTemp(filepath.Dir(path), ".tmp-artifact-*")
	if err != nil {
		return fmt.Errorf("create temp artifact: %w", err)
	}
	tmpPath := tmpFile.Name()
	defer func() {
		_ = tmpFile.Close()
		_ = os.Remove(tmpPath)
	}()
	if _, err := io.Copy(tmpFile, r); err != nil {
		return fmt.Errorf("write artifact content: %w", err)
	}
	if err := tmpFile.Sync(); err != nil {
		return fmt.Errorf("sync artifact file: %w", err)
	}
	if err := tmpFile.Close(); err != nil {
		return fmt.Errorf("close artifact file: %w", err)
	}
	if err := os.Rename(tmpPath, path); err != nil {
		return fmt.Errorf("rename artifact file to target: %w", err)
	}
	return nil
}

func (s *DiskStore) Open(_ context.Context, ref string) (io.ReadCloser, error) {
	path, err := s.resolve(ref)
	if err != nil {
		return nil, err
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open disk artifact %q: %w", path, err)
	}
	return f, nil
}

func (s *DiskStore) resolve(ref string) (string, error) {
	if ref == "" || filepath.IsAbs(ref) {
		return "", fmt.Errorf("invalid disk artifact ref %q", ref)
	}
	path := filepath.Join(s.root, filepath.Clean(ref))
	rel, err := filepath.Rel(s.root, path)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return "", fmt.Errorf("artifact ref %q escapes disk root %q", ref, s.root)
	}
	return path, nil
}

type s3API interface {
	PutObject(ctx context.Context, params *s3.PutObjectInput, optFns ...func(*s3.Options)) (*s3.PutObjectOutput, error)
	GetObject(ctx context.Context, params *s3.GetObjectInput, optFns ...func(*s3.Options)) (*s3.GetObjectOutput, error)
}

type S3Store struct {
	client s3API
	bucket string
}

func NewS3Store(client s3API, bucket string) (*S3Store, error) {
	if client == nil {
		return nil, fmt.Errorf("s3 client is required")
	}
	if bucket == "" {
		return nil, fmt.Errorf("artifact S3 bucket is required")
	}
	return &S3Store{client: client, bucket: bucket}, nil
}

func (s *S3Store) Put(ctx context.Context, ref string, r io.Reader) error {
	if err := validateObjectRef(ref); err != nil {
		return err
	}
	_, err := s.client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(ref),
		Body:   r,
	})
	if err != nil {
		return fmt.Errorf("s3 put object: %w", err)
	}
	return nil
}

func (s *S3Store) Open(ctx context.Context, ref string) (io.ReadCloser, error) {
	if err := validateObjectRef(ref); err != nil {
		return nil, err
	}
	res, err := s.client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(ref),
	})
	if err != nil {
		return nil, fmt.Errorf("s3 get object: %w", err)
	}
	return res.Body, nil
}

func validateObjectRef(ref string) error {
	if ref == "" || strings.HasPrefix(ref, "/") || strings.Contains(ref, "://") {
		return fmt.Errorf("invalid artifact ref %q", ref)
	}
	return nil
}

type Stores struct {
	order  []string
	stores map[string]Store
}

func NewStores(cfg spindleconfig.ArtifactStores, diskFallback, legacyS3Bucket string) (*Stores, error) {
	stores := &Stores{stores: make(map[string]Store)}
	diskDir := cfg.Disk.Dir
	if diskDir == "" {
		diskDir = diskFallback
	}
	if diskDir != "" {
		disk, err := NewDiskStore(diskDir)
		if err != nil {
			return nil, err
		}
		stores.order = append(stores.order, "disk")
		stores.stores["disk"] = disk
	}

	bucket := cfg.S3.Bucket
	if bucket == "" {
		bucket = legacyS3Bucket
	}
	if bucket != "" {
		awsCfg, err := config.LoadDefaultConfig(context.Background(), config.WithRegion(cfg.S3.Region))
		if err != nil {
			return nil, fmt.Errorf("load aws config: %w", err)
		}
		s3Store, err := NewS3Store(s3.NewFromConfig(awsCfg), bucket)
		if err != nil {
			return nil, err
		}
		stores.order = append(stores.order, "s3")
		stores.stores["s3"] = s3Store
	}
	return stores, nil
}

func (s *Stores) Names() []string {
	return append([]string(nil), s.order...)
}

func (s *Stores) Store(name string) (Store, bool) {
	store, ok := s.stores[name]
	return store, ok
}

func (s *Stores) Open(ctx context.Context, ref string) (io.ReadCloser, error) {
	var errs []error
	for _, name := range s.order {
		rc, err := s.stores[name].Open(ctx, ref)
		if err == nil {
			return rc, nil
		}
		errs = append(errs, fmt.Errorf("%s: %w", name, err))
	}
	return nil, fmt.Errorf("open artifact %q: %w", ref, errors.Join(errs...))
}

func (s *Stores) PutFile(ctx context.Context, ref, sourcePath string) []error {
	var errs []error
	for _, name := range s.order {
		store := s.stores[name]
		if disk, ok := store.(*DiskStore); ok {
			target, err := disk.resolve(ref)
			if err == nil {
				source, sourceErr := filepath.Abs(sourcePath)
				targetAbs, targetErr := filepath.Abs(target)
				if sourceErr == nil && targetErr == nil && source == targetAbs {
					continue
				}
			}
		}
		file, err := os.Open(sourcePath)
		if err != nil {
			errs = append(errs, fmt.Errorf("%s: open source: %w", name, err))
			continue
		}
		err = store.Put(ctx, ref, file)
		_ = file.Close()
		if err != nil {
			errs = append(errs, fmt.Errorf("%s: %w", name, err))
		}
	}
	return errs
}
