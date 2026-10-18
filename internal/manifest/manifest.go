// SPDX-License-Identifier: Apache-2.0

// Package manifest is the oracle record for a bench object, shared with the Rust
// side (crates/adit-client/src/oracle.rs): per-part SHA-256 digests at a fixed
// part size, the whole-object SHA-256, and the S3-style composite
// base64(sha256(d1 || ... || dn)) + "-n" that S3 itself stores for a multipart
// object uploaded with SHA-256 checksums.
package manifest

import (
	"bufio"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"os"
)

// Object is one line of a manifest JSONL file.
type Object struct {
	Bucket    string   `json:"bucket"`
	Key       string   `json:"key"`
	Size      int64    `json:"size"`
	SHA256    string   `json:"sha256"`
	PartSize  int64    `json:"part_size"`
	Parts     []string `json:"parts"`
	Composite string   `json:"composite"`
	Seed      *uint64  `json:"seed,omitempty"`
}

// PlanEntry is a manifest line plus a presigned GET URL, as adit-get reads it.
type PlanEntry struct {
	Object
	URL string `json:"url"`
}

// Composite returns S3's composite checksum string for raw part digests.
func Composite(parts [][32]byte) string {
	h := sha256.New()
	for _, d := range parts {
		h.Write(d[:])
	}
	return fmt.Sprintf("%s-%d", base64.StdEncoding.EncodeToString(h.Sum(nil)), len(parts))
}

// Read parses a JSONL manifest.
func Read(path string) ([]Object, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer func() { _ = f.Close() }()
	return Decode(f)
}

// Decode parses JSONL manifest lines from r.
func Decode(r io.Reader) ([]Object, error) {
	var out []Object
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 1<<20), 64<<20)
	for n := 1; sc.Scan(); n++ {
		line := sc.Bytes()
		if len(line) == 0 {
			continue
		}
		var o Object
		if err := json.Unmarshal(line, &o); err != nil {
			return nil, fmt.Errorf("manifest line %d: %w", n, err)
		}
		out = append(out, o)
	}
	return out, sc.Err()
}
