# Ordinal-zero native qualification input

Status: SOURCE-WIRED COMPARISON, ORIGINAL ISSUER MISSING, NATIVE UNOBSERVED.
No admission gate is opened. This supplements source_map.md and D's strict
private cleanup init-v2 comparison contract; shutdown/retired remain v1.
The comparison fields add no authority, resource pool or host-idleness claim.

## Actual source trace and correction

Pinned tinygrad 4c206a52b1a72a98db8c97576959b54fa2a38232
runtime/ops_cuda.py:104 obtains CUdevice for the selected ordinal; line105 passes
that handle to cuCtxCreate_v2; line106 instead passes the ordinal integer to
cuDeviceComputeCapability. The generated API expects CUdevice for both calls.
The lifecycle wrapper now forwards its retained successful cuDeviceGet handle
to the capability query, while still validating the pinned caller's selected
ordinal. An injected driver returning handle37 for ordinal0 exercises the actual
CUDADevice constructor, context creation, capability query and shutdown. The
vendor checkout is unchanged. No native equality of handle and ordinal is assumed.

Lillux crates/kernel/lillux/src/character_device.rs observes retained O_PATH,
NOFOLLOW character-device identities (role, destination, access, major/minor).
Its observation-contract digest hashes supplied facts and device observations;
that hash does not independently observe those facts. The application resource
pool in crates/daemon/ryeos-app/src/execution_resources.rs refuses nonempty
semantic facts without a qualified admitted host observer. There is currently
no qualified UUID-v2/PCI/ordinal observer joined to these retained descriptors.
An isolated visible count of one consequently cannot prove admitted hardware.

CUDA Driver 12.8.1 device API reference:
https://docs.nvidia.com/cuda/archive/12.8.1/cuda-driver-api/group__CUDA__DEVICE.html
cuDeviceGet returns a handle for an ordinal; UUID-v2 identifies the GPU or MIG
compute instance. PCI address, name, old UUID, environment text and ordinal alone
are not substitutes for that independently observed identity.

## Existing operation and required native inputs

Reuse the existing GNU qualifier/worker-launch and observation-contract machinery.
Do not add a daemon, device-discovery system, host-wide idle probe or alternate
cleanup channel. Runtime source now requires a strict init-v2 UUID comparison
before installing lifecycle; this check is a qualification input, not
authorization to execute the native operation.

The native operation must retain all of the following before driver initialization:

- The exact admitted node generation and selected CharacterDeviceSet descriptors,
  existing resource-set and observation-contract bindings.
- A qualified independent node observation of the selected device UUID-v2 as
  exactly32 lowercase hex characters (16 octets), joined to those same retained device identities.
  This observer and its qualification are presently MISSING.
- Exact selected GNU/CUDA bindings, driver/nvproxy/native closure and operation
  authority; original process owner, capsule, deadline and financial/cleanup
  reservation. The comparison UUID and a digest are not this authority.

Native result acceptance also needs consumer verification against that independent
observation and the same live handle used for context creation. A standalone
diagnostic followed by a second cuDeviceGet is insufficient: product wiring must
check UUID-v2 immediately after the selected get and before creating the context,
then retain that verified handle. Lifecycle now prepares this exact same-handle
comparison behind optional `install(expected_uuid_v2_hex=...)`: the guarded
cuDeviceGet queries UUID-v2 internally before returning to the pinned device
constructor, and mismatch/query error poisons before context creation. Direct
UUID queries stay forbidden. This is comparison code, not an admitted observer.
Runtime now passes the required init-v2 UUID comparison to lifecycle install,
which checks the actual returned handle before context creation. It does not
run the separate diagnostic helper first. D still has no independently admitted
same-node UUID/provider observation or original-owner issuer for that frame.
The source is wired, but the admitted input and native proof are MISSING;
GPU admission stays closed.

## Callable diagnostic input/result and refusals

`qualify_ordinal_zero(bindings=selected_generated_bindings,
expected_uuid_v2_hex=independent_observation)` resolves the finite query surface,
then calls cuInit(0), cuDeviceGetCount, cuDeviceGet(ordinal0), and
cuDeviceGetUuid_v2(actual_returned_handle). It creates no context/allocation and
loads no library itself. Invoke only under the retained native-operation authority,
before installing lifecycle guards. Its caller owns deadline and retirement.

Success returns frozen DeviceBindingObservation with only visible_device_count=1,
selected_ordinal=0 and the matching uuid_v2_hex. This is diagnostic data, not an
admitted observation, signed result, cleanup receipt or admission capability.
Failure raises DeviceBindingError and must refuse the qualification. Missing,
noncanonical or zero expected UUID refuses before driver use; missing UUID-v2 API
or incorrect16-byte layout refuses before initialization; native exceptions/error
statuses, visible count other than1 and UUID mismatch refuse without context
creation or identity fallback. Installed lifecycle guards refuse this helper and
still forbid direct UUID queries. No helper return implies physical exclusivity.

Successful native qualification of the complete product additionally needs real
kernel/copy/output behavior and orderly synchronization, event/module/device and
portable-host release, context destruction, existing retired+EOF observation,
then D's independent group retirement and financial settlement under the original
reservation. These results remain UNOBSERVED. The focused controls use actual
pinned generated bindings and CUDADevice with an injected DLL, prohibit native
CDLL loading, and establish source/refusal behavior only. Earlier passed suites
were not repeated for this follow-up.
