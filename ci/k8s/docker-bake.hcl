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
