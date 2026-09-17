//! Platform facts observed at the host-mechanism boundary.
//!
//! Higher layers compare these opaque, portable labels but never select or
//! reproduce the operating-system mechanism used to obtain them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformTarget {
    pub os: &'static str,
    pub arch: &'static str,
}

pub fn current_target() -> PlatformTarget {
    PlatformTarget {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}
