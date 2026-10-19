//! The node a test application stands on: a node Store opened through the
//! native gate (State, Runtime and Operational on the shared test host, under
//! the App root's operator lock), the node identity, the node configuration
//! over the App root, and the write barrier the Store was opened with.
//!
//! It opens the Store through the same owner a live node uses, so a test
//! never holds a stock connection. The node signing key stays inside the
//! identity and the signer. The accessors here return the identity itself,
//! never key bytes or tokens.
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::state_store::NodeIdentitySigner;
use crate::state_store::StateStore;
use crate::state_store::test_native::TestStateStore;
use crate::write_barrier::WriteBarrier;

/// The node pieces a test application is assembled from, apart from the
/// Store owner. Their `Arc`s of the Store are clones of the owner's.
pub struct NativeTestNode {
    config: ryeos_node_runtime::config::Config,
    identity: ryeos_node_runtime::identity::NodeIdentity,
    write_barrier: WriteBarrier,
}

impl NativeTestNode {
    pub fn config(&self) -> &ryeos_node_runtime::config::Config {
        &self.config
    }

    pub fn identity(&self) -> &ryeos_node_runtime::identity::NodeIdentity {
        &self.identity
    }

    pub fn write_barrier(&self) -> &WriteBarrier {
        &self.write_barrier
    }

    /// The configuration, identity and write barrier, by value.
    pub fn into_parts(
        self,
    ) -> (
        ryeos_node_runtime::config::Config,
        ryeos_node_runtime::identity::NodeIdentity,
        WriteBarrier,
    ) {
        let Self {
            config,
            identity,
            write_barrier,
        } = self;
        (config, identity, write_barrier)
    }
}

/// A node Store owner with the node pieces built over one App root. It
/// stays on the thread that built it; every clone of the Store must be gone
/// before it closes.
pub struct NativeTestState {
    store: TestStateStore,
    node: NativeTestNode,
}

impl NativeTestState {
    /// Create the node identity and operator key under `app_root`, then open
    /// the node Store over it. The caller owns `app_root` and keeps it alive
    /// until this state is dropped or closed.
    pub fn open(app_root: &Path) -> Result<Self> {
        let key_path = app_root.join("identity").join("node-key.pem");
        let config = ryeos_node_runtime::config::Config {
            bind: "127.0.0.1:0".parse().context("test bind address")?,
            db_path: TestStateStore::runtime_state_dir(app_root).join("runtime.sqlite3"),
            uds_path: app_root.join("test.sock"),
            app_root: app_root.to_path_buf(),
            node_signing_key_path: key_path.clone(),
            operator_signing_key_path: app_root.join("user-key.pem"),
            authorized_keys_dir: app_root.join("auth"),
        };
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&key_path)
            .context("create test node identity")?;
        ryeos_node_runtime::identity::NodeIdentity::create(&config.operator_signing_key_path)
            .context("create test operator identity")?;
        let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let write_barrier = WriteBarrier::new();
        let store = TestStateStore::open_with_barrier(
            app_root,
            signer,
            write_barrier.clone(),
            Arc::new(head_trust),
        )?;
        Ok(Self {
            store,
            node: NativeTestNode {
                config,
                identity,
                write_barrier,
            },
        })
    }

    /// The shared Store. Clones must be dropped before this state closes.
    pub fn store(&self) -> &Arc<StateStore> {
        self.store.store()
    }

    pub fn config(&self) -> &ryeos_node_runtime::config::Config {
        &self.node.config
    }

    pub fn identity(&self) -> &ryeos_node_runtime::identity::NodeIdentity {
        &self.node.identity
    }

    pub fn write_barrier(&self) -> &WriteBarrier {
        &self.node.write_barrier
    }

    /// A lease on the operator lock this state holds, for the controller
    /// lifetime of an application built over the Store.
    pub fn controller_lifetime(&self) -> Arc<ryeos_node_runtime::state_lock::StateLockLease> {
        self.store.controller_lifetime()
    }

    /// Split into the Store owner, which keeps the Store open and closes it
    /// on drop, and the node pieces. Hold the owner until every clone of the
    /// Store is dropped.
    pub fn into_parts(self) -> (TestStateStore, NativeTestNode) {
        let Self { store, node } = self;
        (store, node)
    }

    /// Close the Store into the census, reporting a refused close.
    pub fn close(self) -> Result<()> {
        let Self { store, node: _ } = self;
        store.close()
    }
}
