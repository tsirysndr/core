package engine

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

type S3 struct {
	bucket string
	client *s3.Client
}

const BaseS3Path = "spindle/workflows"

func NewS3(bucket string) (*S3, error) {
	if bucket == "" {
		return nil, fmt.Errorf("s3 bucket not provided")
	}

	ctx := context.Background()
	sdkConfig, err := config.LoadDefaultConfig(ctx)

	if err != nil {
		return nil, fmt.Errorf("error loading s3 config: %w", err)
	}
	s3Client := s3.NewFromConfig(sdkConfig)

	return &S3{
		bucket: bucket,
		client: s3Client,
	}, nil
}

func (s *S3) WriteFile(ctx context.Context, path string) error {
	s3Key := fmt.Sprintf("%s/%s", BaseS3Path, filepath.Base(path))

	file, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("error opening file %s: %w", path, err)
	}
	defer file.Close()

	_, err = s.client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: &s.bucket,
		Key:    &s3Key,
		Body:   file,
	})

	if err != nil {
		return fmt.Errorf("error writing to s3: %w", err)
	}

	return nil
}

func (s *S3) ReadFile(ctx context.Context, name string) ([]byte, error) {
	res, err := s.client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: &s.bucket,
		Key:    aws.String(name),
	})

	if err != nil {
		return nil, fmt.Errorf("error reading file %s: %w", name, err)
	}
	defer res.Body.Close()

	return io.ReadAll(res.Body)
}
