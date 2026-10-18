// SPDX-License-Identifier: Apache-2.0

package schema

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func write(t *testing.T, dir, name, body string) string {
	t.Helper()
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestRepoSchemasLoad(t *testing.T) {
	s, err := Load("../../bench/schema")
	if err != nil {
		t.Fatal(err)
	}
	for _, n := range []string{"paths", "g0c", "capabilities"} {
		if len(s[n]) == 0 {
			t.Fatalf("schema %s missing", n)
		}
	}
}

func TestCheckEnforcesLaws(t *testing.T) {
	d := t.TempDir()
	schemas := map[string][]string{"paths": strings.Split("path,gbps,host_cpu_s,proc_cpu_s,host_mem_read_bytes,host_mem_write_bytes,efa_rx_bytes,oracle,config,commit,notes", ",")}
	good := write(t, d, "paths-v0.csv", "# instance g7e.12xlarge\npath,gbps,host_cpu_s,proc_cpu_s,host_mem_read_bytes,host_mem_write_bytes,efa_rx_bytes,oracle,config,commit,notes\nE,100,0.4,0.2,unavailable,unavailable,123,ok,chunk=8,abc,\nA,20,,,,,,ok,s5cmd,abc,\n")
	if p := Check(good, schemas); len(p) != 0 {
		t.Fatalf("good file flagged: %v", p)
	}
	bad := write(t, d, "paths-bad.csv", "path,gbps,host_cpu_s,proc_cpu_s,host_mem_read_bytes,host_mem_write_bytes,efa_rx_bytes,oracle,config,commit,notes\nE,100,,0.2,1,1,1,maybe,,abc,\n")
	p := strings.Join(Check(bad, schemas), "\n")
	for _, want := range []string{"law 1", "law 3", "oracle \"maybe\""} {
		if !strings.Contains(p, want) {
			t.Errorf("missing %q in %s", want, p)
		}
	}
	if p := Check(write(t, d, "mystery.csv", "a\n"), schemas); len(p) != 1 {
		t.Fatalf("unmatched file not flagged: %v", p)
	}
}
