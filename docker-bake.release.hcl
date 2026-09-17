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

variable "WORKLOAD_CLIENT_ARTIFACT_DIR" {
  default = "./workload-client-release-artifacts"
}

variable "CONTAINED_OCI_HOOK_ARTIFACT_DIR" {
  default = "./contained-oci-hook-release-artifacts"
}

variable "STANDARD_TAG" {
  default = "ryeos-standard:release-candidate"
}

variable "CENTRAL_HOST_TAG" {
  default = "ryeos-central-host:release-candidate"
}

variable "LOCAL_INFERENCE_TAG" {
  default = "ryeos-local-inference:release-candidate"
}

variable "HOSTED_WORKFLOW_TAG" {
  default = "ryeos-hosted-workflow:release-candidate"
}

variable "CONTAINED_WORKFLOW_TAG" {
  default = "ryeos-contained-workflow:qualification-candidate"
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

target "workload-client-artifact" {
  inherits = ["_release"]
  target   = "workload-client-artifact"
  output   = ["type=local,dest=${WORKLOAD_CLIENT_ARTIFACT_DIR}"]
}

# Qualification-only until installed host evidence has passed. This target is
# deliberately absent from the official promotion workflow.
target "contained-oci-hook-artifact" {
  inherits = ["_release"]
  target   = "contained-oci-hook-artifact"
  output   = ["type=local,dest=${CONTAINED_OCI_HOOK_ARTIFACT_DIR}"]
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

target "local-inference" {
  inherits = ["_release"]
  target   = "ryeos-local-inference"
  tags     = [LOCAL_INFERENCE_TAG]
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

# Qualification-only until the selected adapter/host evidence is accepted.
target "contained-workflow" {
  inherits = ["_release"]
  target   = "ryeos-contained-workflow"
  tags     = [CONTAINED_WORKFLOW_TAG]
  output   = ["type=docker"]
}
