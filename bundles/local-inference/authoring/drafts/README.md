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
