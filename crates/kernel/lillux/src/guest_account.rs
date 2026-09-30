//! Exact non-root account for a disposable guest's trusted owner process.
//!
//! Bundle policy selects the coordinate; only Lillux interprets native UID,
//! GID, ownership, and the irreversible in-process credential transition.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{ControllerAccount, PinnedDirectory, PinnedRegularFile};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestRuntimeAccount {
    Unix { uid: u32, gid: u32 },
}

impl GuestRuntimeAccount {
    pub fn validate(&self) -> Result<()> {
        let Self::Unix { uid, gid } = *self;
        ControllerAccount::unix(uid, gid)
            .validate()
            .map_err(anyhow::Error::msg)
    }

    pub fn grant_private_directory(&self, directory: &PinnedDirectory) -> Result<()> {
        self.controller_account()?
            .grant_private_directory(directory)
    }

    pub fn grant_private_file(&self, file: &PinnedRegularFile) -> Result<()> {
        self.controller_account()?.grant_private_file(file)
    }

    pub fn require_current_process(&self) -> Result<()> {
        self.controller_account()?.require_current_process()
    }

    /// Convert the current one-shot owner process, never a parent process or
    /// model-controlled child. Root must be relinquished before guest content
    /// observation, native namespace preparation, or controller attachment.
    pub fn drop_current_process(&self) -> Result<()> {
        self.validate()?;
        #[cfg(target_os = "linux")]
        {
            let Self::Unix { uid, gid } = *self;
            ensure!(
                unsafe { libc::geteuid() } == 0,
                "guest owner is not root before account transition"
            );
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
                || unsafe { libc::setgroups(0, std::ptr::null()) } != 0
                || unsafe { libc::setresgid(gid, gid, gid) } != 0
                || unsafe { libc::setresuid(uid, uid, uid) } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            self.require_current_process()?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("guest owner account transition is unavailable on this OS")
    }

    fn controller_account(&self) -> Result<ControllerAccount> {
        self.validate()?;
        let Self::Unix { uid, gid } = *self;
        Ok(ControllerAccount::unix(uid, gid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_account_requires_exact_non_root_native_identity() {
        for (uid, gid) in [(0, 65534), (65534, 0), (u32::MAX, 65534)] {
            assert!(GuestRuntimeAccount::Unix { uid, gid }.validate().is_err());
        }
        assert!(
            GuestRuntimeAccount::Unix {
                uid: 65534,
                gid: 65534
            }
            .validate()
            .is_ok()
        );
    }
}
