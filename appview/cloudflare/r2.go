package cloudflare

import (
	"bytes"
	"context"
	"fmt"
	"mime"
	"net/http"
	"path/filepath"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	s3types "github.com/aws/aws-sdk-go-v2/service/s3/types"
)

// SyncFiles uploads the given files (keyed by relative path) to R2 under
// prefix and deletes any objects from a previous deploy that are no longer
// present. It is a pure R2 operation — callers are responsible for
// constructing the prefix and fetching the source files before calling this.
func (cl *Client) SyncFiles(ctx context.Context, prefix string, files map[string][]byte) error {
	existingKeys, err := cl.listR2Objects(ctx, prefix)
	if err != nil {
		return fmt.Errorf("listing existing R2 objects: %w", err)
	}

	for relPath, content := range files {
		key := prefix + relPath
		_, err := cl.s3.PutObject(ctx, &s3.PutObjectInput{
			Bucket:      aws.String(cl.bucket),
			Key:         aws.String(key),
			Body:        bytes.NewReader(content),
			ContentType: aws.String(DetectContentType(relPath, content)),
		})
		if err != nil {
			return fmt.Errorf("uploading %q: %w", key, err)
		}
	}

	for existingKey := range existingKeys {
		relPath := strings.TrimPrefix(existingKey, prefix)
		if _, kept := files[relPath]; !kept {
			if err := cl.deleteR2Object(ctx, existingKey); err != nil {
				return fmt.Errorf("deleting orphan %q: %w", existingKey, err)
			}
		}
	}

	return nil
}

// DeleteFiles removes all R2 objects under the given prefix.
func (cl *Client) DeleteFiles(ctx context.Context, prefix string) error {
	keys, err := cl.listR2Objects(ctx, prefix)
	if err != nil {
		return fmt.Errorf("listing R2 objects for deletion: %w", err)
	}
	for key := range keys {
		if err := cl.deleteR2Object(ctx, key); err != nil {
			return fmt.Errorf("deleting %q: %w", key, err)
		}
	}
	return nil
}

// listR2Objects returns all object keys in the bucket under the given prefix,
// handling pagination automatically.
func (cl *Client) listR2Objects(ctx context.Context, prefix string) (map[string]struct{}, error) {
	keys := make(map[string]struct{})
	var continuationToken *string

	for {
		out, err := cl.s3.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
			Bucket:            aws.String(cl.bucket),
			Prefix:            aws.String(prefix),
			ContinuationToken: continuationToken,
		})
		if err != nil {
			return nil, err
		}

		for _, obj := range out.Contents {
			if obj.Key != nil {
				keys[*obj.Key] = struct{}{}
			}
		}

		if !aws.ToBool(out.IsTruncated) {
			break
		}
		continuationToken = out.NextContinuationToken
	}

	return keys, nil
}

func (cl *Client) deleteR2Object(ctx context.Context, key string) error {
	_, err := cl.s3.DeleteObject(ctx, &s3.DeleteObjectInput{
		Bucket: aws.String(cl.bucket),
		Key:    aws.String(key),
	})
	return err
}

// deleteBatch deletes up to 1000 objects in a single call.
// Unused for now, kept for future bulk-delete optimisation.
func (cl *Client) deleteBatch(ctx context.Context, keys []string) error {
	if len(keys) == 0 {
		return nil
	}

	var objects []s3types.ObjectIdentifier
	for _, k := range keys {
		k := k
		objects = append(objects, s3types.ObjectIdentifier{Key: &k})
	}

	_, err := cl.s3.DeleteObjects(ctx, &s3.DeleteObjectsInput{
		Bucket: aws.String(cl.bucket),
		Delete: &s3types.Delete{Objects: objects},
	})
	return err
}

// DetectContentType guesses the MIME type from the file extension, falling
// back to sniffing the first 512 bytes of content.
func DetectContentType(relPath string, content []byte) string {
	if ext := filepath.Ext(relPath); ext != "" {
		if mt := mime.TypeByExtension(ext); mt != "" {
			return mt
		}
	}

	sniff := content
	if len(sniff) > 512 {
		sniff = sniff[:512]
	}
	return http.DetectContentType(sniff)
}
