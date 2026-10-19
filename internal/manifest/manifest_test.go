// SPDX-License-Identifier: Apache-2.0

package manifest

import (
	"crypto/sha256"
	"encoding/base64"
	"strings"
	"testing"
)

// The same vector as the Rust oracle test: parts "a" and "b".
func TestCompositeMatchesS3Format(t *testing.T) {
	a, b := sha256.Sum256([]byte("a")), sha256.Sum256([]byte("b"))
	h := sha256.New()
	h.Write(a[:])
	h.Write(b[:])
	want := base64.StdEncoding.EncodeToString(h.Sum(nil)) + "-2"
	if got := Composite([][32]byte{a, b}); got != want {
		t.Fatalf("Composite = %q, want %q", got, want)
	}
}

func TestDecodeSkipsBlankLinesAndNamesBadLine(t *testing.T) {
	in := `{"bucket":"b","key":"k","size":1,"sha256":"x","part_size":8,"parts":["x"],"composite":"y-1"}

not json
`
	_, err := Decode(strings.NewReader(in))
	if err == nil || !strings.Contains(err.Error(), "line 3") {
		t.Fatalf("want line 3 error, got %v", err)
	}
}
