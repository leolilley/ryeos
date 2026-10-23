//! Restored-guest qualification of the guest owner wire. The origin role runs
//! under the node as an admitted verifier; the guest role runs inside the
//! restored sandbox. Both live in one executable so the delivered guest bytes
//! are exactly the bytes the signed policy names.

pub mod evidence;
pub mod guest;
pub mod loopback_tls;
pub mod origin;
