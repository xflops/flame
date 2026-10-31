variable "IMAGE_REGISTRY" {
  default = "xflops"
}

variable "IMAGE_TAG" {
  default = "ci"
}

group "default" {
  targets = [
    "session-manager",
    "object-cache",
    "executor-manager",
    "console",
  ]
}

target "image" {
  context = "."
}

target "session-manager" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.fsm"
  tags       = ["${IMAGE_REGISTRY}/flame-session-manager:${IMAGE_TAG}"]
}

target "object-cache" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.foc"
  tags       = ["${IMAGE_REGISTRY}/flame-object-cache:${IMAGE_TAG}"]
}

target "executor-manager" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.fem"
  tags       = ["${IMAGE_REGISTRY}/flame-executor-manager:${IMAGE_TAG}"]
}

target "console" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.console"
  tags       = ["${IMAGE_REGISTRY}/flame-console:${IMAGE_TAG}"]
}

# The -devel executor image (C toolchain and Python headers for GPU applications), built from the
# executor-manager target so a PR proves docker/Dockerfile.fem-devel builds; not part of the default group.
target "executor-manager-devel" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.fem-devel"
  contexts = {
    flame-executor-manager = "target:executor-manager"
  }
  tags = ["${IMAGE_REGISTRY}/flame-executor-manager:${IMAGE_TAG}-devel"]
}

# The CRI shim E2E runtime image; not part of the default group.
target "runtime" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.flmrt"
  tags       = ["${IMAGE_REGISTRY}/flmrt:${IMAGE_TAG}"]
}

# The -devel runtime image, built from the runtime target so a PR proves docker/Dockerfile.flmrt-devel
# builds; not part of the default group.
target "runtime-devel" {
  inherits   = ["image"]
  dockerfile = "docker/Dockerfile.flmrt-devel"
  contexts = {
    flame-instance-runtime = "target:runtime"
  }
  tags = ["${IMAGE_REGISTRY}/flmrt:${IMAGE_TAG}-devel"]
}
