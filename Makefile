# SPDX-License-Identifier: Apache-2.0
#
# Everything repeatable is a target. Rust builds against the NIXL stub API unless you use
# build-real (on an instance with NIXL at $(NIXL_PREFIX)), which refuses to fall back.

NIXL_PREFIX ?= /opt/nvidia/nvda_nixl
AWS_PROFILE ?= aws
export AWS_PROFILE

# macOS: the NIXL stub links -lstdc++, which macOS lacks (libc++ stands in), and the PyO3
# extension resolves Python symbols at load time.
ifeq ($(shell uname -s),Darwin)
SHIM := $(HOME)/.xtmp/adit-macshim
export RUSTFLAGS := -L $(SHIM) -C link-arg=-undefined -C link-arg=dynamic_lookup
endif

FLEET ?= g0
N ?= 1
P ?= E
WORKLOAD ?= T1

.PHONY: help build build-real test test-hw lint fmt schema-check probe fleet fleet-down fleet-status bench report teardown shim

help: ## list targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-14s %s\n", $$1, $$2}'

shim:
ifeq ($(shell uname -s),Darwin)
	@mkdir -p $(SHIM) && cp -n "$$(xcrun --show-sdk-path)/usr/lib/libc++.tbd" $(SHIM)/libstdc++.tbd 2>/dev/null || true
endif

build: shim ## Rust workspace (stub NIXL) and aditctl
	cargo build --workspace --features stub-api
	CGO_ENABLED=0 go build -trimpath -o bin/aditctl ./cmd/aditctl

build-real: ## Rust binaries against the real NIXL (instances only)
	NIXL_PREFIX=$(NIXL_PREFIX) NIXL_NO_STUBS_FALLBACK=1 cargo build --release -p adit-proxy -p adit-client

test: shim ## unit and integration tests (no AWS, no GPU)
	cargo test --workspace --features stub-api
	go test -race ./...

test-hw: ## hardware tests (GPU, real NIXL) on an instance
	NIXL_PREFIX=$(NIXL_PREFIX) NIXL_NO_STUBS_FALLBACK=1 cargo test --release -p adit-client -- --ignored

lint: shim ## fmt check, clippy -D warnings, go vet, golangci-lint
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --features stub-api -- -D warnings
	go vet ./...
	golangci-lint run ./...

fmt: ## format Rust and Go
	cargo fmt --all
	gofmt -w cmd internal

schema-check: ## bench/results against bench/schema and the laws
	go run ./cmd/aditctl report --check bench/results

probe: ## G0b single-box facts (run on a box)
	bash hack/probe.sh

fleet: ## place FLEET with N proxies (set CANDIDATES, TTL, CLIENT_CAP, PROXY_CAP)
	go run ./cmd/aditctl fleet up --name $(FLEET) --proxies $(N) \
		$(if $(CANDIDATES),--candidates $(CANDIDATES)) $(if $(TTL),--ttl $(TTL)) \
		--client-cost-limit $(CLIENT_CAP) --proxy-cost-limit $(PROXY_CAP)

fleet-status: ## instances of FLEET
	go run ./cmd/aditctl fleet status --name $(FLEET)

fleet-down: ## terminate FLEET, verify via spawn list, delete its security group
	go run ./cmd/aditctl fleet down --name $(FLEET)

bench: ## run path P on WORKLOAD (on the client box)
	bash bench/paths/$(P)/run.sh $(WORKLOAD)

report: schema-check ## results check (comparison table arrives with G3 data)

teardown: ## assert no adit instance remains (spawn list)
	go run ./cmd/aditctl teardown
