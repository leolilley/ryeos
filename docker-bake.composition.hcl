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
# Registry that persists build cache across runs. The reference is a plain
# repository; each target stores its cache under its own tag. An empty value
# disables the cache, as in a local build.
variable "BUILD_CACHE_REF" { default = "" }

group "composition" {
  targets = ["core", "core-scheduling", "core-workflow", "core-knowledge", "standard", "central-host", "local-inference", "hosted-workflow", "bundle-source", "f5r-controller"]
}

# Dependency compilation lives in an image layer (see Dockerfile.release), and a
# registry cache exports image layers but not cache mounts. Every target also
# reads the controller's cache: all targets share the dependency layers, so any
# one warm cache restores them.
function "cache_from" {
  params = [name]
  result = BUILD_CACHE_REF == "" ? [] : distinct([
    "type=registry,ref=${BUILD_CACHE_REF}:${name}",
    "type=registry,ref=${BUILD_CACHE_REF}:f5r-controller",
  ])
}

function "cache_to" {
  params = [name]
  result = BUILD_CACHE_REF == "" ? [] : [
    "type=registry,ref=${BUILD_CACHE_REF}:${name},mode=max,image-manifest=true,oci-mediatypes=true,compression=zstd",
  ]
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
  cache-from = cache_from("core")
  cache-to   = cache_to("core")
  target   = "ryeos-core"
  tags     = ["${IMAGE_PREFIX}-core:${IMAGE_TAG}"]
}

target "core-scheduling" {
  inherits = ["_composition"]
  cache-from = cache_from("core-scheduling")
  cache-to   = cache_to("core-scheduling")
  target   = "ryeos-core-scheduling"
  tags     = ["${IMAGE_PREFIX}-core-scheduling:${IMAGE_TAG}"]
}

target "core-workflow" {
  inherits = ["_composition"]
  cache-from = cache_from("core-workflow")
  cache-to   = cache_to("core-workflow")
  target   = "ryeos-core-workflow"
  tags     = ["${IMAGE_PREFIX}-core-workflow:${IMAGE_TAG}"]
}

target "core-knowledge" {
  inherits = ["_composition"]
  cache-from = cache_from("core-knowledge")
  cache-to   = cache_to("core-knowledge")
  target   = "ryeos-core-knowledge"
  tags     = ["${IMAGE_PREFIX}-core-knowledge:${IMAGE_TAG}"]
}

target "standard" {
  inherits = ["_composition"]
  cache-from = cache_from("standard")
  cache-to   = cache_to("standard")
  target   = "ryeos-standard"
  tags     = ["${IMAGE_PREFIX}-standard:${IMAGE_TAG}"]
}

target "central-host" {
  inherits = ["_composition"]
  cache-from = cache_from("central-host")
  cache-to   = cache_to("central-host")
  target   = "ryeos-central-host"
  tags     = ["${IMAGE_PREFIX}-central-host:${IMAGE_TAG}"]
}

target "local-inference" {
  inherits = ["_composition"]
  cache-from = cache_from("local-inference")
  cache-to   = cache_to("local-inference")
  target   = "ryeos-local-inference"
  tags     = ["${IMAGE_PREFIX}-local-inference:${IMAGE_TAG}"]
}

target "hosted-workflow" {
  inherits = ["_composition"]
  cache-from = cache_from("hosted-workflow")
  cache-to   = cache_to("hosted-workflow")
  target   = "ryeos-hosted-workflow"
  tags     = ["${IMAGE_PREFIX}-hosted-workflow:${IMAGE_TAG}"]
}

target "bundle-source" {
  inherits = ["_composition"]
  cache-from = cache_from("bundle-source")
  cache-to   = cache_to("bundle-source")
  target   = "ryeos-substrate"
  tags     = ["${IMAGE_PREFIX}-bundle-source:${IMAGE_TAG}"]
}

target "f5r-controller" {
  inherits = ["_composition"]
  cache-from = cache_from("f5r-controller")
  cache-to   = cache_to("f5r-controller")
  target   = "ryeos-f5r-controller"
  tags     = ["${IMAGE_PREFIX}-f5r-controller:${IMAGE_TAG}"]
}
