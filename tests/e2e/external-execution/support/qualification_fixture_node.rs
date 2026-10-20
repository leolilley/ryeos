//! Runtime-tree and scratch-node helpers used only by the qualification
//! fixture, which builds three synthetic runtime trees and runs them on a
//! throwaway node.

use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::qualification_node::{ScratchSink, large_capture_policy, manifest_hash, no_ignore};

/// Executables a joined synthetic runtime tree is made of. The caller names
/// them because only a test target knows its own `CARGO_BIN_EXE_*` paths.
pub struct RuntimeBinaries<'a> {
    pub candidate: &'a Path,
    pub provider: &'a Path,
}

/// Populate one synthetic runtime tree. `marker` adds one extra regular file so
/// two trees built from the same binaries have distinct manifest identities.
pub fn install_runtime_files(runtime: &Path, binaries: &RuntimeBinaries<'_>, marker: Option<&str>) {
    std::fs::create_dir_all(runtime.join("bin")).unwrap();
    for (source, destination) in [
        (binaries.candidate, runtime.join("bin/candidate")),
        (binaries.provider, runtime.join("bin/provider")),
    ] {
        std::fs::copy(source, &destination).unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    lillux::platform::install_native_runtime_libraries(
        runtime,
        &[binaries.candidate, binaries.provider],
    )
    .unwrap();
    if let Some(marker) = marker {
        let destination = runtime.join("bin/marker");
        std::fs::write(&destination, marker.as_bytes()).unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// A private runtime directory with its owner-private tightening applied.
pub fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    lillux::PinnedDirectory::open(directory.path())
        .unwrap()
        .unwrap()
        .tighten_owner_private_directory()
        .unwrap();
    directory
}

/// Exercise the production large-content capture and object store for one
/// runtime tree, leaving its bytes resident in the node.
pub fn install_large_runtime_content(
    state: &ryeos_app::state::AppState,
    runtime: &lillux::PinnedDirectory,
) -> ryeos_state::objects::ExternalLargeContentManifestObject {
    let authority = state.state_store.pinned_state_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let large = authority.large_object_store().unwrap();
    let ignore = no_ignore();
    let mut sink = ScratchSink { cas, large };
    let manifest =
        ryeos_state::capture_large_tree(runtime, &large_capture_policy(&ignore), &mut sink)
            .unwrap();
    let stored = sink
        .cas
        .store_object(&manifest.to_value().unwrap())
        .unwrap();
    assert_eq!(stored, manifest_hash(&manifest));
    manifest
}

/// Run one async scenario on the thread and runtime shape the qualification
/// verifiers need. The node Store is opened and closed on that thread, after
/// the runtime has ended every task that could still hold the application.
pub fn run_on_test_node<F, Fut>(scenario: F)
where
    F: FnOnce(PathBuf, ryeos_app::state::AppState) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    std::thread::Builder::new()
        .name("synthetic-runtime-qualification".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap();
            let root = tempfile::tempdir().unwrap();
            let (store_owner, state) =
                ryeos_app::state::test_support::build_bound(root.path()).unwrap();
            // A failing scenario must still end its runtime's tasks and close
            // the Store, so one failure does not leave the native host dirty.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(scenario(root.path().to_owned(), state))
            }));
            drop(runtime);
            let closed = store_owner.close();
            match outcome {
                Ok(()) => closed.unwrap(),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
