<!-- ryeos:signed:2026-07-21T00:24:55Z:eb2e6afbc40e15c1eac4f022e98bea8f9aaa5c9253357473d50ea2dc526eb480:gxicErFlsYkcMJvI5fcHyqfPVUr0FNuokeKllN1qlImVR2+8iWIEw9NN4n045QDeKEyPJYoJHd33uLLWsNTtDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: filesystem-durability
title: Filesystem Durability and Platform Guarantees
description: Exact atomicity, durability, and crash-recovery guarantees of RyeOS filesystem primitives
entry_type: reference
version: "1.0.0"
```

# Filesystem Durability and Platform Guarantees

This document defines what RyeOS means when a filesystem mutation is called
*atomic* or *durable*. It covers the primitives exported by `lillux` and their
CAS, vault, node-configuration, and bundle callers. SQLite has separate
transaction and journaling semantics and is outside this matrix.

The strongest guarantees assume a local filesystem that implements
same-filesystem rename atomically and honors file and directory `fsync`, plus
storage that correctly reports completed flushes. Network, userspace, overlay,
and unusual filesystems may be weaker. RyeOS cannot compensate for a
filesystem, kernel, hypervisor, or device that lies about persistence.

## Vocabulary

- **Atomic visibility**: observers see the old namespace entry or the new one,
  never a partially written file or a remove-then-create gap.
- **Content durable on return**: regular-file bytes passed a successful flush
  barrier before the call returned.
- **Namespace durable on return**: the containing directory passed a
  successful flush after creation, rename, or removal.
- **Crash recoverable**: a higher-level journal can converge after a process or
  machine crash. Independent filesystem calls are not thereby one transaction.
- **Private creation**: secret bytes are first written to a newly created
  `0600` temporary file rather than written permissively and chmodded later.

## Primitive matrix

| Primitive | Linux | Other Unix | Non-Unix | Limits |
|---|---|---|---|---|
| `atomic_write` | Atomic same-filesystem replacement; temporary-file content and parent directory are flushed | Same | Temporary-file content is flushed, then rename is attempted; replacement and namespace durability are platform/filesystem dependent | Creating missing parents is not followed by flushing every ancestor. Rename semantics come from the mounted filesystem. |
| `atomic_write_private` | `openat` with `O_EXCL`, `O_NOFOLLOW`, and `0600`; file and already-open final parent are flushed | Same | Falls back to `atomic_write`; no `0600`, dirfd, or final-parent no-follow guarantee | Ancestor symlinks remain supported. On Unix the final parent must be a real directory. This is not confidentiality from another process already able to read as the same user. |
| `atomic_write_batch` | Each target is temp-renamed, then one `syncfs` barrier is issued on the first target's filesystem | Resulting files are flushed after all renames; renamed parent directories are not separately flushed | Same as other non-Linux platforms | The batch is **not atomic**. A crash may install a prefix or leave missing/empty files. Linux durability covers every target only when all targets share the first target's filesystem; callers must enforce that invariant. |
| `rename_path_durable` | Requires siblings; rename then parent-directory flush | Same | Rename only; parent-directory flush is not implemented | It does not flush source contents. Flush a staged file/tree first. |
| `atomic_exchange_paths` | Atomic sibling exchange with `renameat2(RENAME_EXCHANGE)`, then parent-directory flush | Unsupported; returns an error | Unsupported; returns an error | Requires Linux kernel and filesystem support. There is deliberately no remove-then-rename fallback. |
| `remove_file_durable` | Remove then parent-directory flush | Same | Remove only | Missing is success. It does not securely erase blocks or snapshots. |
| `remove_dir_all_durable` | Recursive removal then removed root's parent is flushed | Same | Recursive removal only | Recursive removal is not atomic; a crash or error may leave a partial tree. |
| `sync_tree_durable` | Flushes every regular file bottom-up and every directory; does not follow symlinks | Same | Flushes regular files; directory-entry flushes are not implemented | Rejects a symlink root and special entries. Concurrent mutation can invalidate the completed traversal. |
| `ExclusiveFileLock` and `with_exclusive_file_lock` | Advisory `flock` on a sibling `0600`, no-follow lock file | Same | No interprocess exclusion; runs unlocked | Every cooperating writer must use the same derived lock. This is not a boundary against non-cooperating processes. |

## Higher-level contracts

CAS uses a reachability rule: immutable objects are flushed before a durable
signed head or reference advances. Batch writes are safe there only because
partial pre-head objects are unreachable and current callers keep each batch
on one filesystem.

Vault and private-key writes use private atomic replacement on Unix. Multi-file
key rotation is not a filesystem transaction; its journal and backups provide
recovery across individually durable replacements.

Bundle installation flushes a staged tree before activation. First install
uses durable sibling rename. Replacement requires Linux atomic exchange and
fails closed elsewhere. Registration and tree activation are separate durable
mutations coordinated by a recovery journal, not simultaneously atomic. Old
generation cleanup is recursive and may need journal-driven retry.

Projection databases are rebuildable read models. CAS/head persistence and
SQLite projection updates are intentionally not one filesystem transaction;
the signed head is authoritative and cursor mismatch drives repair.

## Exclusions

RyeOS does not guarantee atomicity across filesystems or a group of renamed
files, directory durability where directory flushing is not implemented,
secure deletion from SSDs/snapshots/backups, interprocess locking on non-Unix,
or safety from code that bypasses the shared locks and journals.

New durable workflows must name their reachability or commit point, keep
pre-commit data on the same filesystem, and define recovery for every later
mutation.
