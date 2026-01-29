# Detect OS and set container runtime
UNAME_S := $(shell uname -s)
ifeq ($(UNAME_S),Darwin)
    CONTAINER_RUNTIME ?= podman
else
    CONTAINER_RUNTIME ?= docker
endif

# Docker image configuration
DOCKER_REGISTRY ?= xflops
FSM_TAG ?= $(shell cargo get --entry session_manager/ package.version --pretty)
FEM_TAG ?= $(shell cargo get --entry executor_manager/ package.version --pretty)
CONSOLE_TAG ?= latest

# Docker image names
FSM_IMAGE = $(DOCKER_REGISTRY)/flame-session-manager
FEM_IMAGE = $(DOCKER_REGISTRY)/flame-executor-manager
CONSOLE_IMAGE = $(DOCKER_REGISTRY)/flame-console

# Dockerfile paths
FSM_DOCKERFILE = docker/Dockerfile.fsm
FEM_DOCKERFILE = docker/Dockerfile.fem
CONSOLE_DOCKERFILE = docker/Dockerfile.console

# Installation configuration
INSTALL_PREFIX ?= /tmp/flame-dev
FLAME_ENDPOINT ?= http://127.0.0.1:8080

# Default target
.PHONY: help build build-release docker-build docker-push docker-release docker-clean update_protos init sdk-go-build sdk-go-test sdk-go-clean e2e e2e-py e2e-py-docker e2e-py-local e2e-local e2e-rs format format-rust format-python install install-dev uninstall uninstall-dev start-services stop-services

help: ## Show this help message
	@echo "Available targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

build: update_protos ## Build the Rust project
	cargo build

build-release: update_protos ## Build the Rust project in release mode
	cargo build --release

init: ## Install required tools
	cargo install cargo-get --force

# Installation targets using flmadm
install: build-release ## Install Flame to system (requires sudo)
	sudo ./target/release/flmadm install --src-dir . --skip-build --enable

install-dev: build-release ## Install Flame to dev location (no sudo required)
	./target/release/flmadm install --src-dir . --skip-build --no-systemd --prefix $(INSTALL_PREFIX)
	@echo ""
	@echo "Flame installed to: $(INSTALL_PREFIX)"
	@echo "Add to PATH: export PATH=$(INSTALL_PREFIX)/bin:\$$PATH"
	@echo "Start services manually:"
	@echo "  $(INSTALL_PREFIX)/bin/flame-session-manager --config $(INSTALL_PREFIX)/conf/flame-cluster.yaml &"
	@echo "  $(INSTALL_PREFIX)/bin/flame-executor-manager --config $(INSTALL_PREFIX)/conf/flame-cluster.yaml &"

uninstall: ## Uninstall Flame from system (requires sudo)
	sudo ./target/release/flmadm uninstall --force

uninstall-dev: ## Uninstall Flame from dev location
	./target/release/flmadm uninstall --prefix $(INSTALL_PREFIX) --no-backup --force || true

start-services: ## Start Flame services (systemd)
	sudo systemctl start flame-session-manager flame-executor-manager

stop-services: ## Stop Flame services (systemd)
	sudo systemctl stop flame-executor-manager flame-session-manager

update_protos: ## Update protobuf files
	@cp rpc/protos/frontend.proto sdk/rust/protos
	@cp rpc/protos/types.proto sdk/rust/protos
	@cp rpc/protos/shim.proto sdk/rust/protos
	@echo "Copied protobuf files to sdk/rust/protos"

	@cp rpc/protos/frontend.proto sdk/python/protos
	@cp rpc/protos/types.proto sdk/python/protos
	@cp rpc/protos/shim.proto sdk/python/protos
	@echo "Copied protobuf files to sdk/python/protos"

sdk-python-generate: update_protos ## Generate the Python protobuf files
	cd sdk/python && make build-protos

sdk-python-test: update_protos ## Test the Python SDK
	cd sdk/python && make test

sdk-python-clean: ## Clean Python SDK build artifacts
	cd sdk/python && make clean

sdk-python: sdk-python-generate sdk-python-test ## Build and test the Python SDK

# Formatting targets
format-rust: ## Format Rust code with cargo fmt
	cargo fmt

format-python: ## Format Python code with ruff
	cd sdk/python && make format

format: format-rust format-python ## Format both Rust and Python code

# E2E testing targets
e2e-py: ## Run Python E2E tests (use e2e-py-docker for docker compose or e2e-py-local for local cluster)
	@echo "Use 'make e2e-py-docker' for docker compose tests or 'make e2e-py-local' for local cluster tests"

e2e-py-docker: ## Run Python E2E tests with docker compose
	$(CONTAINER_RUNTIME) compose exec -w /opt/e2e flame-console uv run -n pytest -vv --durations=0 .

e2e-py-local: ## Run Python E2E tests against local cluster
	cd e2e && FLAME_ENDPOINT=$(FLAME_ENDPOINT) uv run pytest -vv --durations=0 .

e2e-rs: ## Run Rust E2E tests
	cargo test --workspace --exclude cri-rs -- --nocapture

e2e: e2e-py-docker e2e-rs ## Run all E2E tests (Python and Rust) with docker compose

e2e-local: e2e-py-local e2e-rs ## Run all E2E tests against local cluster

# Docker build targets
docker-build-fsm: update_protos ## Build session manager Docker image
	$(CONTAINER_RUNTIME) build -t $(FSM_IMAGE):$(FSM_TAG) -f $(FSM_DOCKERFILE) .
	$(CONTAINER_RUNTIME) tag $(FSM_IMAGE):$(FSM_TAG) $(FSM_IMAGE):latest

docker-build-fem: update_protos ## Build executor manager Docker image
	$(CONTAINER_RUNTIME) build -t $(FEM_IMAGE):$(FEM_TAG) -f $(FEM_DOCKERFILE) .
	$(CONTAINER_RUNTIME) tag $(FEM_IMAGE):$(FEM_TAG) $(FEM_IMAGE):latest

docker-build-console: update_protos ## Build console Docker image
	$(CONTAINER_RUNTIME) build -t $(CONSOLE_IMAGE):$(CONSOLE_TAG) -f $(CONSOLE_DOCKERFILE) .

docker-build-cache: update_protos ## Build object cache Docker image
	$(CONTAINER_RUNTIME) build -t $(CACHE_IMAGE):$(CACHE_TAG) -f $(CACHE_DOCKERFILE) .
	$(CONTAINER_RUNTIME) tag $(CACHE_IMAGE):$(CACHE_TAG) $(CACHE_IMAGE):latest

docker-build: docker-build-fsm docker-build-fem docker-build-console docker-build-cache ## Build all Docker images

# Docker push targets
docker-push-fsm: docker-build-fsm ## Push session manager Docker image
	$(CONTAINER_RUNTIME) push $(FSM_IMAGE):$(FSM_TAG)
	$(CONTAINER_RUNTIME) push $(FSM_IMAGE):latest

docker-push-fem: docker-build-fem ## Push executor manager Docker image
	$(CONTAINER_RUNTIME) push $(FEM_IMAGE):$(FEM_TAG)
	$(CONTAINER_RUNTIME) push $(FEM_IMAGE):latest

docker-push-console: docker-build-console ## Push console Docker image
	$(CONTAINER_RUNTIME) push $(CONSOLE_IMAGE):$(CONSOLE_TAG)

docker-push: docker-push-fsm docker-push-fem docker-push-console ## Push all Docker images

# Release targets
docker-release: init docker-build docker-push ## Build and push all images for release

ci-image: update_protos ## Build images for CI (without version tags)
	$(CONTAINER_RUNTIME) build -t $(FSM_IMAGE) -f $(FSM_DOCKERFILE) .
	$(CONTAINER_RUNTIME) build -t $(FEM_IMAGE) -f $(FEM_DOCKERFILE) .
	$(CONTAINER_RUNTIME) build -t $(CONSOLE_IMAGE) -f $(CONSOLE_DOCKERFILE) .

# Cleanup targets
docker-clean: ## Remove all flame Docker images
	$(CONTAINER_RUNTIME) rmi $(FSM_IMAGE):$(FSM_TAG) $(FSM_IMAGE):latest 2>/dev/null || true
	$(CONTAINER_RUNTIME) rmi $(FEM_IMAGE):$(FEM_TAG) $(FEM_IMAGE):latest 2>/dev/null || true
	$(CONTAINER_RUNTIME) rmi $(CONSOLE_IMAGE):$(CONSOLE_TAG) 2>/dev/null || true

docker-clean-all: ## Remove all Docker images and containers (use with caution)
	$(CONTAINER_RUNTIME) system prune -a -f

# Development targets
docker-run-fsm: docker-build-fsm ## Run session manager container
	$(CONTAINER_RUNTIME) run --rm -it $(FSM_IMAGE):latest

docker-run-fem: docker-build-fem ## Run executor manager container
	$(CONTAINER_RUNTIME) run --rm -it $(FEM_IMAGE):latest

docker-run-console: docker-build-console ## Run console container
	$(CONTAINER_RUNTIME) run --rm -it $(CONSOLE_IMAGE):latest

# Utility targets
docker-images: ## List all flame Docker images
	$(CONTAINER_RUNTIME) images | grep $(DOCKER_REGISTRY)/flame

docker-logs: ## Show logs for running flame containers
	$(CONTAINER_RUNTIME) ps | grep flame | awk '{print $$1}' | xargs -I {} $(CONTAINER_RUNTIME) logs {}

# Legacy targets for backward compatibility
docker-release-legacy: init ## Legacy release target (original implementation)
	$(CONTAINER_RUNTIME) build -t $(FSM_IMAGE):$(FSM_TAG) -f $(FSM_DOCKERFILE) .
	$(CONTAINER_RUNTIME) build -t $(FEM_IMAGE):$(FEM_TAG) -f $(FEM_DOCKERFILE) .

