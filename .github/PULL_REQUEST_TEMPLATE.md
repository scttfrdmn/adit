## Summary

<!-- What does this change and why. -->

## Related issue

<!-- e.g. Closes #12 / Refs #12 -->

## Checklist

- [ ] Commits follow Conventional Commits
- [ ] **CHANGELOG updated** under `[Unreleased]`
- [ ] `make lint` is clean (`cargo fmt --check`, `clippy -D warnings`, `go vet`, `golangci-lint`)
- [ ] `make test` passes and new code has tests
- [ ] No network or AWS access in unit tests; hardware tests are `#[ignore]`d and tagged
- [ ] New files carry the `SPDX-License-Identifier: Apache-2.0` header
- [ ] Any number in this PR has its counters, configuration and commit beside it (CLAUDE.md law 1)
