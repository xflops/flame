# Detect a usable Docker-compatible container CLI.
DETECTED_CONTAINER_CLI := $(shell if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then echo docker; elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then echo podman; elif command -v docker >/dev/null 2>&1; then echo docker; elif command -v podman >/dev/null 2>&1; then echo podman; else echo docker; fi)
ifdef CONTAINER_RUNTIME
CONTAINER_CLI ?= $(CONTAINER_RUNTIME)
else
CONTAINER_CLI ?= $(DETECTED_CONTAINER_CLI)
endif
CONTAINER_RUNTIME ?= $(CONTAINER_CLI)

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

# Release image configuration
IMAGE_REGISTRY ?= docker.io/$(DOCKER_REGISTRY)
DOCKER_TAG ?= $(RELEASE_TAG)
RELEASE_IMAGE_PLATFORMS ?= linux/amd64,linux/arm64
RUST_BUILDER_IMAGE ?= docker.io/library/rust:1.95
UBUNTU_BASE_IMAGE ?= docker.io/library/ubuntu:24.04

# Installation configuration
INSTALL_PREFIX ?= /tmp/flame-dev
FLAME_ENDPOINT ?= http://127.0.0.1:8080
FLAME_ROOT := $(CURDIR)
E2E_SYSTEM_PROFILE ?= all
E2E_SYSTEM_PYTEST_ARGS ?=

# Default target
.PHONY: help build build-release init update_protos
.PHONY: install install-dev uninstall uninstall-dev start-services stop-services
.PHONY: sdk-python sdk-python-generate sdk-python-test sdk-python-clean
.PHONY: format format-rust format-python format-e2e
.PHONY: e2e e2e-local e2e-py e2e-py-docker e2e-py-local e2e-rs
.PHONY: e2e-py-system-docker e2e-py-system-local e2e-py-system-stress
.PHONY: e2e-py-system-longevity e2e-py-system-runner
.PHONY: docker-build docker-build-fsm docker-build-fem docker-build-console
.PHONY: docker-push docker-push-fsm docker-push-fem docker-push-console
.PHONY: docker-release release-sanity ci-image
.PHONY: release-images release-images-build release-images-inspect release-images-push
.PHONY: release-images-pull-bases release-images-check-cli release-images-login
.PHONY: release-images-verify require-release-image-tag
.PHONY: docker-clean docker-clean-all docker-run-fsm docker-run-fem docker-run-console
.PHONY: docker-images docker-logs docker-release-legacy

help: ## Show this help message
	@echo "Available targets:"
	@grep -hE '^[[:alnum:]_.-]+:.*## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*## "}; {printf "\033[36m%-32s\033[0m %s\n", $$1, $$2}'

build: update_protos ## Build the Rust project
	cargo build

build-release: update_protos ## Build the Rust project in release mode
	cargo build --release

init: ## Install required tools
	cargo install cargo-get --force

# Installation targets using flmadm
install: build-release ## Install Flame to system (requires sudo)
	sudo ./target/release/flmadm install --all --src-dir . --skip-build --enable

install-dev: build-release ## Install Flame to dev location (no sudo required)
	./target/release/flmadm install --all --src-dir . --skip-build --no-systemd --prefix $(INSTALL_PREFIX)
	@echo ""
	@echo "Flame installed to: $(INSTALL_PREFIX)"
	@echo "Add to PATH: export PATH=$(INSTALL_PREFIX)/bin:\$$PATH"
	@echo "Start services manually:"
	@echo "  FLAME_HOME=$(INSTALL_PREFIX) $(INSTALL_PREFIX)/bin/flame-object-cache --config $(INSTALL_PREFIX)/conf/flame-cluster.yaml &"
	@echo "  FLAME_HOME=$(INSTALL_PREFIX) $(INSTALL_PREFIX)/bin/flame-session-manager --config $(INSTALL_PREFIX)/conf/flame-cluster.yaml &"
	@echo "  FLAME_HOME=$(INSTALL_PREFIX) $(INSTALL_PREFIX)/bin/flame-executor-manager --config $(INSTALL_PREFIX)/conf/flame-cluster.yaml &"

uninstall: ## Uninstall Flame from system (requires sudo)
	sudo ./target/release/flmadm uninstall --force

uninstall-dev: ## Uninstall Flame from dev location
	./target/release/flmadm uninstall --prefix $(INSTALL_PREFIX) --no-backup --force

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

format-e2e: ## Format E2E code with ruff
	cd e2e && uv run --extra dev ruff format src tests

format: format-rust format-python format-e2e ## Format both Rust, Python and E2E code

# E2E testing targets
e2e-py: ## Run Python E2E tests (use e2e-py-docker for docker compose or e2e-py-local for local cluster)
	@echo "Use 'make e2e-py-docker' for docker compose tests or 'make e2e-py-local' for local cluster tests"

e2e-py-docker: ## Run Python E2E tests with docker compose
	$(CONTAINER_CLI) compose exec -w /opt/e2e flame-console bash -c "source /usr/local/flame/sbin/flmenv.sh && PYTHONPATH=/opt/e2e/src:\$$PYTHONPATH python3 -m pytest -vv --durations=0 ."

e2e-py-local: ## Run Python E2E tests against local cluster (requires flamepy installed via pip)
	cd e2e && PYTHONPATH="$(CURDIR)/e2e/src:$$PYTHONPATH" FLAME_ENDPOINT=$(FLAME_ENDPOINT) pytest -vv --durations=0 .

e2e-py-system-docker: ## Run opt-in Python system tests with docker compose (E2E_SYSTEM_PROFILE=all|stress|longevity|runner)
	$(CONTAINER_CLI) compose exec -w /opt/e2e flame-console bash -c "source /usr/local/flame/sbin/flmenv.sh && FLAME_E2E_SYSTEM_TESTS=$(E2E_SYSTEM_PROFILE) PYTHONPATH=/opt/e2e/src:\$$PYTHONPATH python3 -m pytest -vv --durations=0 tests/test_system.py $(E2E_SYSTEM_PYTEST_ARGS)"

e2e-py-system-local: ## Run opt-in Python system tests against local cluster (E2E_SYSTEM_PROFILE=all|stress|longevity|runner)
	cd e2e && FLAME_E2E_SYSTEM_TESTS=$(E2E_SYSTEM_PROFILE) PYTHONPATH="$(CURDIR)/e2e/src:$$PYTHONPATH" FLAME_ENDPOINT=$(FLAME_ENDPOINT) pytest -vv --durations=0 tests/test_system.py $(E2E_SYSTEM_PYTEST_ARGS)

e2e-py-system-stress: ## Run Python system stress tests with docker compose
	$(MAKE) e2e-py-system-docker E2E_SYSTEM_PROFILE=stress E2E_SYSTEM_PYTEST_ARGS="-m stress"

e2e-py-system-longevity: ## Run Python system longevity tests with docker compose
	$(MAKE) e2e-py-system-docker E2E_SYSTEM_PROFILE=longevity E2E_SYSTEM_PYTEST_ARGS="-m longevity"

e2e-py-system-runner: ## Run Python system Runner tests with docker compose
	$(MAKE) e2e-py-system-docker E2E_SYSTEM_PROFILE=runner E2E_SYSTEM_PYTEST_ARGS="-m runner"

e2e-rs: ## Run Rust E2E tests
	FLAME_ROOT=$(FLAME_ROOT) cargo test --workspace --exclude cri-rs -- --nocapture

e2e: e2e-py-docker e2e-rs ## Run all E2E tests (Python and Rust) with docker compose

e2e-local: e2e-py-local e2e-rs ## Run all E2E tests against local cluster

# Docker build targets
docker-build-fsm: update_protos ## Build session manager Docker image
	$(CONTAINER_CLI) build -t $(FSM_IMAGE):$(FSM_TAG) -f $(FSM_DOCKERFILE) .
	$(CONTAINER_CLI) tag $(FSM_IMAGE):$(FSM_TAG) $(FSM_IMAGE):latest

docker-build-fem: update_protos ## Build executor manager Docker image
	$(CONTAINER_CLI) build -t $(FEM_IMAGE):$(FEM_TAG) -f $(FEM_DOCKERFILE) .
	$(CONTAINER_CLI) tag $(FEM_IMAGE):$(FEM_TAG) $(FEM_IMAGE):latest

docker-build-console: update_protos ## Build console Docker image
	$(CONTAINER_CLI) build -t $(CONSOLE_IMAGE):$(CONSOLE_TAG) -f $(CONSOLE_DOCKERFILE) .

docker-build: docker-build-fsm docker-build-fem docker-build-console ## Build all Docker images

# Docker push targets
docker-push-fsm: docker-build-fsm ## Push session manager Docker image
	$(CONTAINER_CLI) push $(FSM_IMAGE):$(FSM_TAG)
	$(CONTAINER_CLI) push $(FSM_IMAGE):latest

docker-push-fem: docker-build-fem ## Push executor manager Docker image
	$(CONTAINER_CLI) push $(FEM_IMAGE):$(FEM_TAG)
	$(CONTAINER_CLI) push $(FEM_IMAGE):latest

docker-push-console: docker-build-console ## Push console Docker image
	$(CONTAINER_CLI) push $(CONSOLE_IMAGE):$(CONSOLE_TAG)

docker-push: docker-push-fsm docker-push-fem docker-push-console ## Push all Docker images

# Release targets
docker-release: init docker-build docker-push ## Build and push all images for release

release-sanity: ## Run non-publishing release sanity checks
	ci/release/sanity.sh

require-release-image-tag:
	@test -n "$(DOCKER_TAG)" || (echo "DOCKER_TAG must be set, for example DOCKER_TAG=v0.6.0" >&2; exit 1)

release-images-check-cli: ## Check the detected container CLI and amd64 Rust builder image
	$(CONTAINER_CLI) info
	$(CONTAINER_CLI) run --rm --platform linux/amd64 "$(RUST_BUILDER_IMAGE)" rustc -vV

release-images-login: ## Log in to Docker Hub with the detected container CLI
	$(CONTAINER_CLI) login docker.io

release-images-build: require-release-image-tag ## Build local multi-arch release image manifests
	@set -eu; \
	platforms=$$(printf '%s' "$(RELEASE_IMAGE_PLATFORMS)" | tr ',' ' '); \
	build_image() { \
		image="$$1"; \
		dockerfile="$$2"; \
		for platform in $$platforms; do \
			echo "$(CONTAINER_CLI) build --platform $$platform --manifest $(IMAGE_REGISTRY)/$$image:$(DOCKER_TAG) -f $$dockerfile ."; \
			$(CONTAINER_CLI) build --platform "$$platform" --manifest "$(IMAGE_REGISTRY)/$$image:$(DOCKER_TAG)" -f "$$dockerfile" .; \
		done; \
	}; \
	build_image flame-session-manager docker/Dockerfile.fsm; \
	build_image flame-object-cache docker/Dockerfile.foc; \
	build_image flame-executor-manager docker/Dockerfile.fem; \
	build_image flame-console docker/Dockerfile.console

release-images-inspect: require-release-image-tag ## Inspect local release image manifests
	$(CONTAINER_CLI) manifest inspect "$(IMAGE_REGISTRY)/flame-session-manager:$(DOCKER_TAG)"
	$(CONTAINER_CLI) manifest inspect "$(IMAGE_REGISTRY)/flame-object-cache:$(DOCKER_TAG)"
	$(CONTAINER_CLI) manifest inspect "$(IMAGE_REGISTRY)/flame-executor-manager:$(DOCKER_TAG)"
	$(CONTAINER_CLI) manifest inspect "$(IMAGE_REGISTRY)/flame-console:$(DOCKER_TAG)"

release-images-push: require-release-image-tag ## Push release manifest lists
	$(CONTAINER_CLI) manifest push "$(IMAGE_REGISTRY)/flame-session-manager:$(DOCKER_TAG)" "docker://$(IMAGE_REGISTRY)/flame-session-manager:$(DOCKER_TAG)"
	$(CONTAINER_CLI) manifest push "$(IMAGE_REGISTRY)/flame-object-cache:$(DOCKER_TAG)" "docker://$(IMAGE_REGISTRY)/flame-object-cache:$(DOCKER_TAG)"
	$(CONTAINER_CLI) manifest push "$(IMAGE_REGISTRY)/flame-executor-manager:$(DOCKER_TAG)" "docker://$(IMAGE_REGISTRY)/flame-executor-manager:$(DOCKER_TAG)"
	$(CONTAINER_CLI) manifest push "$(IMAGE_REGISTRY)/flame-console:$(DOCKER_TAG)" "docker://$(IMAGE_REGISTRY)/flame-console:$(DOCKER_TAG)"

release-images: require-release-image-tag ## Build, inspect, and push release image manifests
	$(MAKE) release-images-build
	$(MAKE) release-images-inspect
	$(MAKE) release-images-push

release-images-verify: require-release-image-tag ## Verify remote release image manifests include expected platforms
	@set -eu; \
	expected_platforms="$(RELEASE_IMAGE_PLATFORMS)"; \
	check_image() { \
		image="$$1"; \
		echo "$(CONTAINER_CLI) manifest inspect $$image"; \
		$(CONTAINER_CLI) manifest inspect "$$image" | python3 ci/release/check-image-platforms.py "$$image" "$$expected_platforms"; \
	}; \
	check_image "$(IMAGE_REGISTRY)/flame-session-manager:$(DOCKER_TAG)"; \
	check_image "$(IMAGE_REGISTRY)/flame-object-cache:$(DOCKER_TAG)"; \
	check_image "$(IMAGE_REGISTRY)/flame-executor-manager:$(DOCKER_TAG)"; \
	check_image "$(IMAGE_REGISTRY)/flame-console:$(DOCKER_TAG)"

release-images-pull-bases: ## Pull release base images with the detected container CLI
	@set -eu; \
	for platform in $$(printf '%s' "$(RELEASE_IMAGE_PLATFORMS)" | tr ',' ' '); do \
		$(CONTAINER_CLI) pull --platform "$$platform" "$(RUST_BUILDER_IMAGE)"; \
		$(CONTAINER_CLI) pull --platform "$$platform" "$(UBUNTU_BASE_IMAGE)"; \
	done

ci-image: update_protos ## Build images for CI (without version tags)
	$(CONTAINER_CLI) build -t $(FSM_IMAGE) -f $(FSM_DOCKERFILE) .
	$(CONTAINER_CLI) build -t $(FEM_IMAGE) -f $(FEM_DOCKERFILE) .
	$(CONTAINER_CLI) build -t $(CONSOLE_IMAGE) -f $(CONSOLE_DOCKERFILE) .

# Cleanup targets
docker-clean: ## Remove all flame Docker images
	$(CONTAINER_CLI) rmi $(FSM_IMAGE):$(FSM_TAG) $(FSM_IMAGE):latest 2>/dev/null || true
	$(CONTAINER_CLI) rmi $(FEM_IMAGE):$(FEM_TAG) $(FEM_IMAGE):latest 2>/dev/null || true
	$(CONTAINER_CLI) rmi $(CONSOLE_IMAGE):$(CONSOLE_TAG) 2>/dev/null || true

docker-clean-all: ## Remove all Docker images and containers (use with caution)
	$(CONTAINER_CLI) system prune -a -f

# Development targets
docker-run-fsm: docker-build-fsm ## Run session manager container
	$(CONTAINER_CLI) run --rm -it $(FSM_IMAGE):latest

docker-run-fem: docker-build-fem ## Run executor manager container
	$(CONTAINER_CLI) run --rm -it $(FEM_IMAGE):latest

docker-run-console: docker-build-console ## Run console container
	$(CONTAINER_CLI) run --rm -it $(CONSOLE_IMAGE):latest

# Utility targets
docker-images: ## List all flame Docker images
	$(CONTAINER_CLI) images | grep $(DOCKER_REGISTRY)/flame

docker-logs: ## Show logs for running flame containers
	$(CONTAINER_CLI) ps | grep flame | awk '{print $$1}' | xargs -I {} $(CONTAINER_CLI) logs {}

# Legacy targets for backward compatibility
docker-release-legacy: init ## Legacy release target (original implementation)
	$(CONTAINER_CLI) build -t $(FSM_IMAGE):$(FSM_TAG) -f $(FSM_DOCKERFILE) .
	$(CONTAINER_CLI) build -t $(FEM_IMAGE):$(FEM_TAG) -f $(FEM_DOCKERFILE) .
