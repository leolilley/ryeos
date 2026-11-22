# Unadmitted GNU CUDA drafts

These descriptors (the `gnu_cuda_persistent_session` protocol, the graph-free
2048 worker, its model provider, activation and product-relationship configs,
and the smoke directive) cannot be admitted yet. The trusted pooled protocol
requires a resource cleanup contract digest paired with a node resource
authority, and no node profile declares that authority. The engine refuses the
protocol without it, and every protocol descriptor under a bundle's `.ai/` tree
is loaded at node initialisation, so shipping them there would stop a node with
this bundle from initialising.

Paths here mirror their place under `.ai/`. They carry no signature. When the
resource authority and cleanup contract exist, they return under `.ai/` together
with that contract and are signed with the rest of the bundle.

## Unqualified Modal driver chain

The Modal user-driver chain is also unsigned and unqualified:

- `config/ryeos-runtime/modal-user-driver-products.yaml`: the build-products
  recipe for one normalized guest driver projection.
- `config/ryeos-runtime/gnu-cuda-driver-qualification.yaml`: the static
  GNU-first closure and Python ABI qualification policy.
- `graphs/local-inference/modal-user-driver-production.yaml`: the graph that
  normalizes one owner-staged Modal guest capture into a tree product.
- `tools/local-inference/modal-driver-normalize/`: the normalizer tool.
- `tools/local-inference/gnu-cuda-driver-qualification/`: the static closure
  verifier tool.

They make no provider, device or runtime claim and were never signed. They stay
here until the driver projection is qualified, then return under `.ai/` signed
with the rest of the bundle. The offline normalizer test in
`tests/modal_driver_product/` loads the normalizer from this tree.
