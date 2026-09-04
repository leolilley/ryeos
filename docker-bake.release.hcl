variable "VERSION" {
  default = ""
}

variable "SOURCE_DATE_EPOCH" {
  default = ""
}

variable "VCS_REF" {
  default = "unknown"
}

variable "BUILD_DATE" {
  default = "unknown"
}

variable "ARTIFACT_DIR" {
  default = "./release-artifacts"
}

variable "STANDARD_TAG" {
  default = "ryeos-standard:release-candidate"
}

variable "CENTRAL_HOST_TAG" {
  default = "ryeos-central-host:release-candidate"
}

variable "HOSTED_WORKFLOW_TAG" {
  default = "ryeos-hosted-workflow:release-candidate"
}

target "_release" {
  context    = "."
  dockerfile = "Dockerfile.release"
  platforms  = ["linux/amd64"]
  pull       = true
  args = {
    VERSION           = VERSION
    SOURCE_DATE_EPOCH = SOURCE_DATE_EPOCH
    VCS_REF           = VCS_REF
    BUILD_DATE        = BUILD_DATE
  }
  secret = ["id=publisher-key,env=RYEOS_PUBLISHER_KEY"]
  cache-from = ["type=gha,scope=ryeos-release-unified"]
}

target "bundle-artifact" {
  inherits = ["_release"]
  target   = "bundle-artifact"
  output   = ["type=local,dest=${ARTIFACT_DIR}"]
  # Every normal release constructs its immutable archive. Attach the shared
  # cache export only here so one multi-output solve does not upload the same
  # compiled graph once per image.
  cache-to = ["type=gha,scope=ryeos-release-unified,mode=max"]
}

target "standard" {
  inherits = ["_release"]
  target   = "ryeos-standard"
  tags     = [STANDARD_TAG]
  output   = ["type=registry"]
  attest = [
    "type=provenance,mode=max",
    "type=sbom",
  ]
}

target "central-host" {
  inherits = ["_release"]
  target   = "ryeos-central-host"
  tags     = [CENTRAL_HOST_TAG]
  output   = ["type=registry"]
  attest = [
    "type=provenance,mode=max",
    "type=sbom",
  ]
}

target "hosted-workflow" {
  inherits = ["_release"]
  target   = "ryeos-hosted-workflow"
  tags     = [HOSTED_WORKFLOW_TAG]
  output   = ["type=registry"]
  attest = [
    "type=provenance,mode=max",
    "type=sbom",
  ]
}
