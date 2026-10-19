// SPDX-License-Identifier: Apache-2.0

// Package s3x holds the S3 helpers aditctl needs: the region check that must
// pass before any I/O, and presigning.
package s3x

import (
	"context"
	"fmt"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/feature/s3/manager"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

// BucketRegion returns the bucket's region from x-amz-bucket-region.
func BucketRegion(ctx context.Context, c *s3.Client, bucket string) (string, error) {
	return manager.GetBucketRegion(ctx, c, bucket)
}

// CheckRegion fails unless the bucket lives in want (law 7: region check before
// any I/O). Cross-region reads cost $0.02/GB and measure the wrong network.
func CheckRegion(ctx context.Context, c *s3.Client, bucket, want string) error {
	got, err := BucketRegion(ctx, c, bucket)
	if err != nil {
		return fmt.Errorf("region of s3://%s: %w", bucket, err)
	}
	if got != want {
		return fmt.Errorf("s3://%s is in %s but this run is in %s: refusing cross-region I/O", bucket, got, want)
	}
	return nil
}

// Presign returns a presigned GET URL valid for ttl.
func Presign(ctx context.Context, c *s3.Client, bucket, key string, ttl time.Duration) (string, error) {
	p := s3.NewPresignClient(c, s3.WithPresignExpires(ttl))
	r, err := p.PresignGetObject(ctx, &s3.GetObjectInput{Bucket: aws.String(bucket), Key: aws.String(key)})
	if err != nil {
		return "", err
	}
	return r.URL, nil
}
