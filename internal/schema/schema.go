// SPDX-License-Identifier: Apache-2.0

// Package schema checks bench/results/*.csv against bench/schema/*.csv and
// against the laws that can be checked mechanically: every row records its
// configuration and commit (law 3), every adit row carries its counters beside
// its throughput (law 1), and the oracle column only holds known verdicts.
package schema

import (
	"encoding/csv"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// Load reads every bench/schema/<name>.csv header.
func Load(dir string) (map[string][]string, error) {
	files, err := filepath.Glob(filepath.Join(dir, "*.csv"))
	if err != nil {
		return nil, err
	}
	out := map[string][]string{}
	for _, f := range files {
		cols, _, err := read(f)
		if err != nil {
			return nil, err
		}
		out[strings.TrimSuffix(filepath.Base(f), ".csv")] = cols
	}
	return out, nil
}

// read returns the header and data rows, skipping '#' comment lines.
func read(path string) ([]string, [][]string, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, nil, err
	}
	var kept []string
	for _, l := range strings.Split(string(b), "\n") {
		if strings.HasPrefix(l, "#") || strings.TrimSpace(l) == "" {
			continue
		}
		kept = append(kept, l)
	}
	r := csv.NewReader(strings.NewReader(strings.Join(kept, "\n")))
	r.FieldsPerRecord = -1
	var rows [][]string
	for {
		rec, err := r.Read()
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, nil, fmt.Errorf("%s: %w", path, err)
		}
		rows = append(rows, rec)
	}
	if len(rows) == 0 {
		return nil, nil, fmt.Errorf("%s: no header", path)
	}
	return rows[0], rows[1:], nil
}

// SchemaFor maps a results file to its schema by name prefix: paths-v0.csv ->
// paths, g0c.csv -> g0c, capabilities.csv -> capabilities.
func SchemaFor(file string, schemas map[string][]string) (string, bool) {
	base := strings.TrimSuffix(filepath.Base(file), ".csv")
	names := make([]string, 0, len(schemas))
	for n := range schemas {
		names = append(names, n)
	}
	sort.Slice(names, func(i, j int) bool { return len(names[i]) > len(names[j]) })
	for _, n := range names {
		if base == n || strings.HasPrefix(base, n+"-") || strings.HasPrefix(base, n+"_") {
			return n, true
		}
	}
	return "", false
}

var counterCols = []string{"host_cpu_s", "proc_cpu_s", "host_mem_read_bytes", "host_mem_write_bytes", "efa_rx_bytes"}

// Check validates one results file; it returns every problem found.
func Check(file string, schemas map[string][]string) []string {
	name, ok := SchemaFor(file, schemas)
	if !ok {
		return []string{fmt.Sprintf("%s: no schema in bench/schema matches this file name", file)}
	}
	want := schemas[name]
	got, rows, err := read(file)
	if err != nil {
		return []string{err.Error()}
	}
	if strings.Join(got, ",") != strings.Join(want, ",") {
		return []string{fmt.Sprintf("%s: header differs from bench/schema/%s.csv\n  got  %s\n  want %s", file, name, strings.Join(got, ","), strings.Join(want, ","))}
	}
	idx := map[string]int{}
	for i, c := range want {
		idx[c] = i
	}
	var probs []string
	for n, row := range rows {
		at := func(c string) string {
			if i, ok := idx[c]; ok && i < len(row) {
				return row[i]
			}
			return ""
		}
		line := fmt.Sprintf("%s row %d", file, n+1)
		if len(row) != len(want) {
			probs = append(probs, fmt.Sprintf("%s: %d fields, header has %d", line, len(row), len(want)))
			continue
		}
		for _, c := range []string{"config", "commit"} {
			if _, ok := idx[c]; ok && at(c) == "" {
				probs = append(probs, fmt.Sprintf("%s: empty %s (law 3: configuration recorded with every result)", line, c))
			}
		}
		if _, ok := idx["oracle"]; ok {
			switch at("oracle") {
			case "ok", "MISMATCH", "partial", "n/a":
			default:
				probs = append(probs, fmt.Sprintf("%s: oracle %q is not ok|MISMATCH|partial|n/a", line, at("oracle")))
			}
		}
		isAdit := name == "g0c" || (name == "paths" && at("path") == "E")
		if isAdit {
			for _, c := range counterCols {
				if _, ok := idx[c]; ok && at(c) == "" {
					probs = append(probs, fmt.Sprintf("%s: empty %s (law 1: a number without its counters is not a result; write \"unavailable\" if the instance cannot measure it)", line, c))
				}
			}
		}
	}
	return probs
}

// CheckDir validates every CSV under dir.
func CheckDir(dir string, schemas map[string][]string) ([]string, int, error) {
	files, err := filepath.Glob(filepath.Join(dir, "*.csv"))
	if err != nil {
		return nil, 0, err
	}
	var probs []string
	for _, f := range files {
		probs = append(probs, Check(f, schemas)...)
	}
	return probs, len(files), nil
}
