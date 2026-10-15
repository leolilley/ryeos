# Composition images: one image per bundle set with its exact native features.
# These are qualification candidates, so the development publication and the
# development publisher owner are the defaults; an official build names its
# publication stage and owner explicitly. No output is set here: a bake
# without `--push` or an explicit `--set *.output=...` pushes nothing.
# Candidates are deployed by digest, never through a mutable tag.
variable "PUBLICATION_STAGE" { default = "development-publication" }
variable "VERSION" { default = "" }
variable "SOURCE_DATE_EPOCH" { default = "" }
variable "VCS_REF" { default = "unknown" }
variable "BUILD_DATE" { default = "unknown" }
variable "PUBLISHER_OWNER" { default = "RyeOS Development" }
variable "CARGO_BUILD_JOBS" { default = "" }
variable "IMAGE_PREFIX" { default = "ryeos" }
variable "IMAGE_TAG" { default = "composition-candidate" }

group "composition" {
  targets = ["core", "core-scheduling", "standard", "f5r-controller"]
}

target "_composition" {
  context    = "."
  dockerfile = "Dockerfile.release"
  platforms  = ["linux/amd64"]
  args = {
    PUBLICATION_STAGE = PUBLICATION_STAGE
    VERSION           = VERSION
    SOURCE_DATE_EPOCH = SOURCE_DATE_EPOCH
    VCS_REF           = VCS_REF
    BUILD_DATE        = BUILD_DATE
    PUBLISHER_OWNER   = PUBLISHER_OWNER
    CARGO_BUILD_JOBS  = CARGO_BUILD_JOBS
  }
  secret = ["id=publisher-key,env=RYEOS_PUBLISHER_KEY"]
  attest = [
    "type=provenance,mode=max",
    "type=sbom",
  ]
}

target "core" {
  inherits = ["_composition"]
  target   = "ryeos-core"
  tags     = ["${IMAGE_PREFIX}-core:${IMAGE_TAG}"]
}

target "core-scheduling" {
  inherits = ["_composition"]
  target   = "ryeos-core-scheduling"
  tags     = ["${IMAGE_PREFIX}-core-scheduling:${IMAGE_TAG}"]
}

target "standard" {
  inherits = ["_composition"]
  target   = "ryeos-standard"
  tags     = ["${IMAGE_PREFIX}-standard:${IMAGE_TAG}"]
}

target "f5r-controller" {
  inherits = ["_composition"]
  target   = "ryeos-f5r-controller"
  tags     = ["${IMAGE_PREFIX}-f5r-controller:${IMAGE_TAG}"]
}
