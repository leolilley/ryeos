# Local qualification only: explicit dev publisher, local outputs, no registry.
# Use a frozen source context and record its revision before starting the build.
variable "CONTAINED_SOURCE" { default = "." }
variable "VERSION" { default = "" }
variable "VCS_REF" { default = "unknown" }
variable "BUILD_DATE" { default = "unknown" }
variable "SOURCE_DATE_EPOCH" { default = "" }
variable "CONTAINED_DEV_KEY" { default = ".dev-keys/PUBLISHER_DEV.pem" }
variable "CONTAINED_DEV_TAG" { default = "ryeos-contained-workflow:dev-qualification" }
variable "CONTAINED_DEV_HOOK_DIR" { default = ".tmp/contained-dev-hook" }

group "default" {
  targets = ["contained-dev", "contained-dev-hook"]
}

target "_development" {
  context = CONTAINED_SOURCE
  dockerfile = "Dockerfile.release"
  platforms = ["linux/amd64"]
  args = {
    VERSION = VERSION
    VCS_REF = VCS_REF
    BUILD_DATE = BUILD_DATE
    SOURCE_DATE_EPOCH = SOURCE_DATE_EPOCH
    CARGO_BUILD_JOBS = "2"
    PUBLISHER_OWNER = "RyeOS Development"
    CONTAINED_PUBLICATION_STAGE = "development-publication"
  }
  secret = ["id=publisher-key,src=${CONTAINED_DEV_KEY}"]
}

target "contained-dev" {
  inherits = ["_development"]
  target = "ryeos-contained-workflow"
  tags = [CONTAINED_DEV_TAG]
  output = ["type=docker"]
}

target "contained-dev-hook" {
  inherits = ["_development"]
  target = "contained-oci-hook-artifact"
  output = ["type=local,dest=${CONTAINED_DEV_HOOK_DIR}"]
}
