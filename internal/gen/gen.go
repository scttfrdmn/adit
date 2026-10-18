// SPDX-License-Identifier: Apache-2.0

// Package gen generates the toy workloads (T1-T3) deterministically and uploads
// them to the bench bucket as multipart objects with SHA-256 checksums, so S3
// stores a composite checksum the GPU-side oracle can be compared against.
//
// Every part's bytes are a pure function of (seed, object index, part index):
// ChaCha8 keyed by SHA-256 of those three. Parts are generated in parallel and
// any object can be regenerated and re-checked from its manifest's seed alone.
package gen

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"math/rand/v2"
	"sync"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"

	"github.com/scttfrdmn/adit/internal/manifest"
)

// Workload is a toy workload shape.
type Workload struct {
	ID      string
	Objects int
	Size    int64
}

// Toy workloads from the project document. T4 is eight clients reading T1.
var Toy = map[string]Workload{
	"T1": {ID: "T1", Objects: 1, Size: 64 << 30},
	"T2": {ID: "T2", Objects: 1000, Size: 1 << 30},
	"T3": {ID: "T3", Objects: 100000, Size: 16 << 20},
}

// Key names object i of workload w under prefix. Keys spread over 16 hex
// prefixes so S3's per-prefix request rate is not the first thing measured.
func Key(prefix string, w Workload, i int) string {
	h := sha256.Sum256(fmt.Appendf(nil, "%s/%d", w.ID, i))
	return fmt.Sprintf("%s/%s/%x/obj%06d", prefix, w.ID, h[0]>>4, i)
}

// PartBytes fills buf with part `part` of object `obj` under `seed`.
func PartBytes(buf []byte, seed uint64, obj, part int) {
	var k [24]byte
	binary.LittleEndian.PutUint64(k[0:], seed)
	binary.LittleEndian.PutUint64(k[8:], uint64(obj))
	binary.LittleEndian.PutUint64(k[16:], uint64(part))
	key := sha256.Sum256(k[:])
	r := rand.NewChaCha8(key)
	_, _ = r.Read(buf)
}

// Object generates object `obj` in memory-bounded parts and calls emit for each
// part in order. It is the reference used by Upload and by tests.
func Object(size, partSize int64, seed uint64, obj int, emit func(part int, b []byte) error) error {
	buf := make([]byte, partSize)
	for p := 0; int64(p)*partSize < size; p++ {
		n := min(partSize, size-int64(p)*partSize)
		PartBytes(buf[:n], seed, obj, p)
		if err := emit(p, buf[:n]); err != nil {
			return err
		}
	}
	return nil
}

// S3API is the subset of the S3 client Upload uses.
type S3API interface {
	CreateMultipartUpload(context.Context, *s3.CreateMultipartUploadInput, ...func(*s3.Options)) (*s3.CreateMultipartUploadOutput, error)
	UploadPart(context.Context, *s3.UploadPartInput, ...func(*s3.Options)) (*s3.UploadPartOutput, error)
	CompleteMultipartUpload(context.Context, *s3.CompleteMultipartUploadInput, ...func(*s3.Options)) (*s3.CompleteMultipartUploadOutput, error)
	AbortMultipartUpload(context.Context, *s3.AbortMultipartUploadInput, ...func(*s3.Options)) (*s3.AbortMultipartUploadOutput, error)
}

// Upload generates and uploads one object with `parallel` parts in flight and
// returns its manifest. It fails unless S3's stored composite checksum equals
// the one computed locally.
func Upload(ctx context.Context, c S3API, bucket, key string, size, partSize int64, seed uint64, obj, parallel int) (manifest.Object, error) {
	m := manifest.Object{Bucket: bucket, Key: key, Size: size, PartSize: partSize, Seed: &seed}
	cmu, err := c.CreateMultipartUpload(ctx, &s3.CreateMultipartUploadInput{
		Bucket:            &bucket,
		Key:               &key,
		ChecksumAlgorithm: types.ChecksumAlgorithmSha256,
		ChecksumType:      types.ChecksumTypeComposite,
	})
	if err != nil {
		return m, fmt.Errorf("create multipart %s: %w", key, err)
	}
	abort := func(err error) (manifest.Object, error) {
		_, _ = c.AbortMultipartUpload(context.WithoutCancel(ctx), &s3.AbortMultipartUploadInput{Bucket: &bucket, Key: &key, UploadId: cmu.UploadId})
		return m, err
	}

	nparts := int((size + partSize - 1) / partSize)
	digests := make([][32]byte, nparts)
	done := make([]types.CompletedPart, nparts)
	// Whole-object SHA-256 must see parts in order; workers hand buffers to an
	// in-order hasher through per-part channels, bounded by the semaphore.
	ready := make([]chan []byte, nparts)
	for i := range ready {
		ready[i] = make(chan []byte, 1)
	}
	whole := sha256.New()
	hashed := make(chan struct{})
	go func() {
		defer close(hashed)
		for i := range nparts {
			b := <-ready[i]
			if b != nil {
				whole.Write(b)
			}
		}
	}()

	sem := make(chan struct{}, parallel)
	var wg sync.WaitGroup
	var mu sync.Mutex
	var firstErr error
	for p := range nparts {
		sem <- struct{}{}
		wg.Add(1)
		go func(p int) {
			defer wg.Done()
			n := min(partSize, size-int64(p)*partSize)
			buf := make([]byte, n)
			PartBytes(buf, seed, obj, p)
			d := sha256.Sum256(buf)
			digests[p] = d
			sum := base64.StdEncoding.EncodeToString(d[:])
			num := int32(p + 1)
			out, err := c.UploadPart(ctx, &s3.UploadPartInput{
				Bucket: &bucket, Key: &key, UploadId: cmu.UploadId, PartNumber: &num,
				Body: bytes.NewReader(buf), ContentLength: aws.Int64(n),
				ChecksumAlgorithm: types.ChecksumAlgorithmSha256, ChecksumSHA256: &sum,
			})
			ready[p] <- buf
			<-sem
			if err != nil {
				mu.Lock()
				if firstErr == nil {
					firstErr = fmt.Errorf("upload part %d of %s: %w", p+1, key, err)
				}
				mu.Unlock()
				return
			}
			done[p] = types.CompletedPart{PartNumber: &num, ETag: out.ETag, ChecksumSHA256: &sum}
		}(p)
	}
	wg.Wait()
	<-hashed
	if firstErr != nil {
		return abort(firstErr)
	}
	cmp, err := c.CompleteMultipartUpload(ctx, &s3.CompleteMultipartUploadInput{
		Bucket: &bucket, Key: &key, UploadId: cmu.UploadId,
		MultipartUpload: &types.CompletedMultipartUpload{Parts: done},
	})
	if err != nil {
		return abort(fmt.Errorf("complete %s: %w", key, err))
	}
	m.SHA256 = hex.EncodeToString(whole.Sum(nil))
	for _, d := range digests {
		m.Parts = append(m.Parts, hex.EncodeToString(d[:]))
	}
	m.Composite = manifest.Composite(digests)
	if got := aws.ToString(cmp.ChecksumSHA256); got != m.Composite {
		return m, fmt.Errorf("%s: S3 stored checksum %q, computed %q", key, got, m.Composite)
	}
	return m, nil
}
