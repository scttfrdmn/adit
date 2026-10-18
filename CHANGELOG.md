# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Repository, laws and CI.** Apache 2.0 license, README, this changelog, `CLAUDE.md` holding
  the nine laws, and a single CI gate (`.github/workflows/gate.yml`) that runs the Rust tree
  (`cargo fmt`, `clippy`, `cargo test` against the NIXL stub API) and the Go tree (`go vet`,
  `golangci-lint`, `go test -race`) plus the bench CSV schema check.
