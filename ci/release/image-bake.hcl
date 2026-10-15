variable "IMAGE_REGISTRY" { default = "docker.io/xflops" }
variable "IMAGE_TAG" { default = "ci" }
variable "ARCH" { default = "amd64" }
variable "SOURCE_CONTEXT" { default = "." }
variable "SOURCE_SHA" { default = "" }

group "default" {
  targets = ["flame-session-manager", "flame-object-cache", "flame-executor-manager", "flame-console", "flame-test", "flame-instance-runtime"]
}

// Build all images in one invocation so their common Cargo stage is shared.
target "image" {
  context = SOURCE_CONTEXT
  platforms = ["linux/${ARCH}"]
  pull = true
  attest = ["type=provenance,disabled=true"]
  labels = {
    "org.opencontainers.image.revision" = SOURCE_SHA
    "org.opencontainers.image.source" = "https://github.com/xflops/flame"
  }
}

target "flame-session-manager" {
  inherits = ["image"]
  dockerfile = "docker/Dockerfile.fsm"
  tags = ["${IMAGE_REGISTRY}/flame-session-manager:${IMAGE_TAG}-${ARCH}"]
  cache-from = ["type=gha,scope=flame-session-manager-${ARCH}"]
  cache-to = ["type=gha,mode=max,scope=flame-session-manager-${ARCH}"]
}

target "flame-object-cache" {
  inherits = ["image"]
  dockerfile = "docker/Dockerfile.foc"
  tags = ["${IMAGE_REGISTRY}/flame-object-cache:${IMAGE_TAG}-${ARCH}"]
  cache-from = ["type=gha,scope=flame-object-cache-${ARCH}"]
  cache-to = ["type=gha,mode=max,scope=flame-object-cache-${ARCH}"]
}

target "flame-executor-manager" {
  inherits = ["image"]
  dockerfile = "docker/Dockerfile.fem"
  tags = ["${IMAGE_REGISTRY}/flame-executor-manager:${IMAGE_TAG}-${ARCH}"]
  cache-from = ["type=gha,scope=flame-executor-manager-${ARCH}"]
  cache-to = ["type=gha,mode=max,scope=flame-executor-manager-${ARCH}"]
}

target "flame-console" {
  inherits = ["image"]
  dockerfile = "docker/Dockerfile.console"
  tags = ["${IMAGE_REGISTRY}/flame-console:${IMAGE_TAG}-${ARCH}"]
  cache-from = ["type=gha,scope=flame-console-${ARCH}"]
  cache-to = ["type=gha,mode=max,scope=flame-console-${ARCH}"]
}

// flame-test is flame-console plus testing/, built from the console target of
// this same invocation, so it never depends on a published console image.
target "flame-test" {
  inherits = ["image"]
  dockerfile = "docker/Dockerfile.testing"
  contexts = {
    flame-console = "target:flame-console"
    testing = "${SOURCE_CONTEXT}/testing"
  }
  tags = ["${IMAGE_REGISTRY}/flame-test:${IMAGE_TAG}-${ARCH}"]
  cache-from = ["type=gha,scope=flame-test-${ARCH}"]
  cache-to = ["type=gha,mode=max,scope=flame-test-${ARCH}"]
}

target "flame-instance-runtime" {
  inherits = ["image"]
  dockerfile = "docker/Dockerfile.flmrt"
  tags = ["${IMAGE_REGISTRY}/flame-instance-runtime:${IMAGE_TAG}-${ARCH}"]
  cache-from = ["type=gha,scope=flame-instance-runtime-${ARCH}"]
  cache-to = ["type=gha,mode=max,scope=flame-instance-runtime-${ARCH}"]
}
