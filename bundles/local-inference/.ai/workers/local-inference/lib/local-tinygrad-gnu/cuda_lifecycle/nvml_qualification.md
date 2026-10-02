# NVIDIA raw-assignment observer source preparation

Status: SOURCE PREPARED, UNWIRED, NATIVE PROVIDER/OCCURRENCE UNSELECTED.
The helper does not admit facts, authorize contact or modify the CUDA cleanup wire.

## Mechanism and source seam

`nvml_observer.observe_assignment(request=..., bindings=...)` is the product-edge
finite vendor translation. It accepts D's projection of ONE existing complete
ExecutionResourceSelection and its signed primary role. It snapshots one retained
primary character identity, initializes NVML, requires count_v2==1, obtains one
opaque physical handle, positively checks non-MIG handle/current/pending mode,
reads minor and UUID from THAT handle, compares minor with the retained primary,
and shuts down NVML before returning a candidate. Index0 is neither minor0 nor
CUDA ordinal0. Count1 without the primary identity join cannot return a result.

The finite ABI is nvmlInit_v2, nvmlDeviceGetCount_v2,
nvmlDeviceGetHandleByIndex_v2, nvmlDeviceIsMigDeviceHandle,
nvmlDeviceGetMigMode, nvmlDeviceGetMinorNumber, nvmlDeviceGetUUID, nvmlShutdown.
All eight must resolve before initialization. A caller-injected ctypes provider
gets explicit int return, unsigned-int output and pointer-width handle signatures.
There is no pynvml import, name search, CDLL/dlopen, library discovery or fallback.
The pinned tinygrad checkout has no generated NVML module; it remains untouched.
Runtime's current exact DLL resolver permits only selected libc/CUDA; no NVML
entry, provider, load authorization or consumer call was added.

NVML describes the Linux minor as the number in /dev/nvidiaN; its UUID query
returns GPU identity, and NVML enumeration need not match CUDA numbering.
[NVIDIA device query reference](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html).
MIG handle and mode queries distinguish instance handles and current/pending mode;
changing mode may reset/unbind a device.
[NVIDIA MIG reference](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlMultiInstanceGPU.html).
The official Linux device-file implementation uses primary major195.
[NVIDIA device-file source](https://github.com/NVIDIA/nvidia-modprobe/blob/main/modprobe-utils/nvidia-modprobe-utils.c).
These references explain the proposed join; they do not select a native provider,
qualify nvproxy or observe this node. ASCII GPU UUID normalization to16 octets
and its equivalence to CUDA UUID-v2 must pass actual selected-provider qualification.

## D-agreed input and result

Input is exactly `{resource_selection: <whole existing ExecutionResourceSelection>,
primary_device_role: <signed role>}`. That selection keeps stable_id/class,
empty matched_facts, observation_contract_digest/device_binding_digest,
execution_restricted access, character_device_grant enforcement and the exact
existing character_devices list of role/destination/access/major/minor records.
Bindings are injected OUTSIDE JSON by the native operation owner. Expected UUID
is not caller input: it is measured independently of the inference consumer.
The product parser is a strict finite subset, not a new core device vocabulary.

The returned candidate contains exactly version1, stable_resource_id,
device_binding_digest, primary_device (the existing five fields), uuid_v2_hex
(32 lowercase hex characters), and nvml_shutdown="closed". It is bounded detail
for the existing ProductQualificationResult.probe_evidence, NOT that result's
admission/claims/policy envelope. No copied coordinate, comparison digest, request
or candidate authenticates an observation. D independently retains the original
qualification owner, raw descriptor authorities/resource_set, selected provider
bytes, host/provider occurrence, capsule, evidence policy and funding/deadline.
The Python helper neither reconstructs nor extends those authorities.

## Refusal and operation ownership

The selected primary must be the sole primary-range195 identity, canonical
/dev/nvidiaN with matching minor below254 and read_write access. Sorted unique
roles, destinations and major/minor pairs are required; renamed/second primaries,
aliases, control/modeset selected as primary and MIG capability paths refuse.
This is specifically the proposed NVIDIA Linux primary contract, not discovery
of arbitrary device majors. Its mapping in actual Modal nvproxy is unqualified.

Missing provider/API, malformed/foreign/raw selection, authored semantic facts,
multiple/no devices, null handle, MIG/unknown outputs, native errors/exceptions,
minor mismatch, malformed/MIG/zero UUID or unsuccessful shutdown refuse. Native
outputs start unknown so status0 without writing them cannot pass. This first
subset also refuses NOT_SUPPORTED MIG queries: unsupported MIG hardware such as
a selected target returning that status is not silently declared non-MIG. Any
alternative interpretation requires separately qualified exact-provider policy.

After every initialization attempt, including an unknown/error outcome, the
helper attempts exactly one nvmlShutdown. Failed close produces no candidate.
NVML shutdown closes observer bookkeeping only: it proves neither CUDA resource
retirement nor physical idleness. A blocked call has no Python timer/retry or
success path. D's SAME original finite owner retains uncertainty, process/group
and financial settlement obligations; no new supervisor or reservation exists.

The complete host operation reuses existing Lillux execution, held attachment,
retained descriptors and original deadline under existing qualification tooling.
Lillux owns placement, input/output transport, time, interruption and group
retirement; D owns observation policy/acceptance and accounting; this module owns
only vendor query interpretation. Native NVML internals may open NVIDIA nodes,
initialize devices, search providers or start helper operations. Those hidden
effects/transitive providers must be checked under the admitted closure and raw
grant; API names or an injected fixture do not establish host-boundary compliance.
No writes to device modes, device-file creation, modprobe, filesystem, sockets,
clocks, process creation or global utilization probes are implemented here.

## Actual qualification still required

Use the existing GNU qualification tooling and finite measurement launch on the
raw assignment under ORIGINAL funding. Select/verify exact independent observer
product and GNU/native ABI; close the admitted NVML+driver/nvproxy provider tree
and all runtime mappings/searches. A missing-provider negative with an ambient
candidate must fail before device contact. Retain actual CharacterDeviceSet and
same host/provider occurrence; prove primary195:minor/UUID correspondence, strict
non-MIG queries, and NVML-to-CUDA UUID-v2 equivalence. Missing current inputs are
UNSELECTED, not invented paths, identities or qualifications.

D's current resource pool still refuses semantic facts without a qualified
observer and has no UUID acceptance/projection. After qualification, D must bind
the measured facts to that SAME retained occurrence; GPU-runtime must verify the
SAME CUdevice's UUID immediately after cuDeviceGet and before context creation.
A diagnostic plus a later second get does not prove this. Existing cleanup v1
retired+EOF, group retirement and original financial settlement remain required
after real kernel/copy/model use. The observer alone closes none of these gates.

Thirteen new offline controls use fake out-parameter bindings; one uses Python
callbacks through ctypes C ABI trampolines to exercise the explicit signature
branch and a wide opaque handle. CDLL is prohibited. Tests cover actual finite
query/order, authoritative-primary mismatch despite count1, all required APIs,
ambiguous/foreign/MIG assignment, native failures/unknown outputs and failed close.
No native NVML implementation, driver, GPU, weights, provider contact, compiler,
install, build or cloud operation runs; earlier passed suites were not repeated.
