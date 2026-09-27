# External guest occurrence owner

This executable is the dedicated, one-shot process beneath RyeOS's existing
external Worker. It takes only a bounded canonical base64url root-signed
assignment on `--assignment-b64`; the signed import and package are separately
delivered to fixed `/ryeos/activation` files. It measures the installed
`/ryeos/guest-runtime` tree (including `controller-root.hex` and canonical
`guest-owner-profile.json`), verifies both signatures and the exact runtime
manifest, creates one occurrence owner, and uses Lillux to stage, install,
hold, and release the packaged supervisor. The owner retains the supervisor
namespace until natural exit or bounded termination. `SIGHUP` from a lost run
stream is not cancellation; `SIGINT`/`SIGTERM` are.

The runtime profile supplies the exact private-source limits and native owner
timeout. It is content in the runtime tree, not an argument or mutable
environment override. The qualified provider snapshot must independently
bind its installed tree, controller root, filesystem immutability, process
survival, and capacity to the signed placement contract. Those qualifications
are **not yet complete**, and the Render adapter must still return
`SupervisorPending`. This owner has no Kaggle, project signing, provider, or
controller credential.

`ryeos-external-execution::guest_runtime_product` can now create a fresh,
credential-free runtime tree from an admitted owner-executable descriptor,
the controller **public** key, and a validated profile. It returns the content
manifest hash calculated from the exact tree the guest later measures. This
is a source product only: the signed authoring tool, provider snapshot
lineage, restored-tree measurement, and installed qualification remain open.

Inbound import and package files must be current-owner pinned regular files
beneath the owner-private `0700` activation directory. Their modes may be
`0600`, `0640`, or `0644`, with no special or executable bits: only the owner
may write, and the private parent denies other users path traversal. Read
bits do not authorize content; the owner still verifies the signed assignment,
import and exact package bytes before sealing a private source. Render's
upload API does not document its resulting file mode, so an
installed qualification must observe the actual mode and refuse any other
shape. The source tree does not infer this from a successful upload response.

A code-zero native exit proves only Lillux's exact target and namespace
settlement. The controller must separately join the authenticated supervisor
channel, frozen candidate closure, writer-exclusion/export evidence, provider
occurrence termination, and B-owned evaluation before accepting work. Neither
an owner exit code nor a Render run-stream response is a `Ready` or candidate
publication claim.
