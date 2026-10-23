# The controller built from the released, officially signed bundle assets
# rather than from a fresh publication. It is deliberately not a composition
# target: it takes no publisher key, so nothing in the build can sign, and its
# bundles are the bytes a release already shipped. RELEASED_BUNDLES_DIR is a
# directory laid out by the release-bundle materializer; VERSION names the
# release the bundles came from. The image is deployed by digest.
variable "VERSION" { default = "" }
variable "SOURCE_DATE_EPOCH" { default = "" }
variable "VCS_REF" { default = "unknown" }
variable "BUILD_DATE" { default = "unknown" }
variable "CARGO_BUILD_JOBS" { default = "" }
variable "IMAGE_PREFIX" { default = "ryeos" }
variable "RELEASED_BUNDLES_DIR" { default = "" }
variable "OFFICIAL_PUBLISHER_FP" { default = "" }

target "f5r-controller-released" {
  context    = "."
  dockerfile = "Dockerfile.release"
  platforms  = ["linux/amd64"]
  target     = "ryeos-f5r-controller-released"
  contexts = {
    released-bundles = RELEASED_BUNDLES_DIR
  }
  args = {
    VERSION               = VERSION
    SOURCE_DATE_EPOCH     = SOURCE_DATE_EPOCH
    VCS_REF               = VCS_REF
    BUILD_DATE            = BUILD_DATE
    CARGO_BUILD_JOBS      = CARGO_BUILD_JOBS
    OFFICIAL_PUBLISHER_FP = OFFICIAL_PUBLISHER_FP
  }
  tags = ["${IMAGE_PREFIX}-f5r-controller:release-v${VERSION}"]
  attest = [
    "type=provenance,mode=max",
    "type=sbom",
  ]
}
