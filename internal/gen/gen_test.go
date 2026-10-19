// SPDX-License-Identifier: Apache-2.0

package gen

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"io"
	"sync"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/s3"

	"github.com/scttfrdmn/adit/internal/manifest"
)

func TestPartBytesDeterministicAndDistinct(t *testing.T) {
	a, b, c := make([]byte, 4096), make([]byte, 4096), make([]byte, 4096)
	PartBytes(a, 1, 0, 0)
	PartBytes(b, 1, 0, 0)
	PartBytes(c, 1, 0, 1)
	if !bytes.Equal(a, b) {
		t.Fatal("same inputs gave different bytes")
	}
	if bytes.Equal(a, c) {
		t.Fatal("different parts gave identical bytes")
	}
}

// fakeS3 stores parts and computes the composite the way S3 does.
type fakeS3 struct {
	mu    sync.Mutex
	parts map[int32][]byte
}

func (f *fakeS3) CreateMultipartUpload(context.Context, *s3.CreateMultipartUploadInput, ...func(*s3.Options)) (*s3.CreateMultipartUploadOutput, error) {
	f.parts = map[int32][]byte{}
	return &s3.CreateMultipartUploadOutput{UploadId: aws.String("u")}, nil
}

func (f *fakeS3) UploadPart(_ context.Context, in *s3.UploadPartInput, _ ...func(*s3.Options)) (*s3.UploadPartOutput, error) {
	b, _ := io.ReadAll(in.Body)
	f.mu.Lock()
	f.parts[*in.PartNumber] = b
	f.mu.Unlock()
	return &s3.UploadPartOutput{ETag: aws.String("e")}, nil
}

func (f *fakeS3) CompleteMultipartUpload(context.Context, *s3.CompleteMultipartUploadInput, ...func(*s3.Options)) (*s3.CompleteMultipartUploadOutput, error) {
	var ds [][32]byte
	for i := int32(1); int(i) <= len(f.parts); i++ {
		ds = append(ds, sha256.Sum256(f.parts[i]))
	}
	return &s3.CompleteMultipartUploadOutput{ChecksumSHA256: aws.String(manifest.Composite(ds))}, nil
}

func (f *fakeS3) AbortMultipartUpload(context.Context, *s3.AbortMultipartUploadInput, ...func(*s3.Options)) (*s3.AbortMultipartUploadOutput, error) {
	return &s3.AbortMultipartUploadOutput{}, nil
}

func TestUploadManifestMatchesSerialGeneration(t *testing.T) {
	f := &fakeS3{}
	size, part := int64(5<<20+17), int64(1<<20)
	m, err := Upload(context.Background(), f, "b", "k", size, part, 42, 3, 4)
	if err != nil {
		t.Fatal(err)
	}
	whole := sha256.New()
	var ds []string
	if err := Object(size, part, 42, 3, func(_ int, b []byte) error {
		whole.Write(b)
		d := sha256.Sum256(b)
		ds = append(ds, hex.EncodeToString(d[:]))
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	if m.SHA256 != hex.EncodeToString(whole.Sum(nil)) {
		t.Fatal("whole-object SHA-256 differs from serial generation")
	}
	if len(m.Parts) != 6 || m.Parts[5] != ds[5] {
		t.Fatalf("parts = %d, last %s want %s", len(m.Parts), m.Parts[len(m.Parts)-1], ds[5])
	}
	_ = base64.StdEncoding
}

func TestKeysSpreadOverPrefixes(t *testing.T) {
	seen := map[string]bool{}
	for i := range 200 {
		k := Key("adit", Toy["T2"], i)
		seen[k[len("adit/T2/"):len("adit/T2/")+1]] = true
	}
	if len(seen) < 12 {
		t.Fatalf("only %d of 16 prefixes used", len(seen))
	}
}
