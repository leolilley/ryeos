//! `AbsolutePathBuf` — a `PathBuf` that is guaranteed absolute by
//! construction. Used by route-system call sites that take a
//! `project_path` from external input (route YAML at compile time,
//! request body at request time).
//!
//! `Deref<Target = Path>` makes the wrapper transparent for read
//! operations; explicit `into_path_buf()` exposes the inner buf when
//! ownership is needed.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AbsolutePathBuf(PathBuf);

#[derive(Debug, thiserror::Error)]
#[error("path '{0}' must be absolute")]
pub struct NotAbsoluteError(pub String);

impl AbsolutePathBuf {
    pub fn try_new(path: PathBuf) -> Result<Self, NotAbsoluteError> {
        if !path.is_absolute() {
            return Err(NotAbsoluteError(path.display().to_string()));
        }
        Ok(Self(path))
    }

    pub fn try_from_str(path: &str) -> Result<Self, NotAbsoluteError> {
        Self::try_new(PathBuf::from(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl std::ops::Deref for AbsolutePathBuf {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for AbsolutePathBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_new_accepts_absolute() {
        let p = AbsolutePathBuf::try_new(PathBuf::from("/tmp/project")).unwrap();
        assert_eq!(p.as_path(), Path::new("/tmp/project"));
    }

    #[test]
    fn try_new_rejects_relative() {
        let err = AbsolutePathBuf::try_new(PathBuf::from("rel/path")).unwrap_err();
        assert!(err.0.contains("rel/path"));
    }

    #[test]
    fn try_from_str_accepts_absolute() {
        let p = AbsolutePathBuf::try_from_str("/opt/proj").unwrap();
        assert_eq!(p.as_path(), Path::new("/opt/proj"));
    }

    #[test]
    fn try_from_str_rejects_relative() {
        let err = AbsolutePathBuf::try_from_str("no/slash").unwrap_err();
        assert!(err.0.contains("no/slash"));
    }

    #[test]
    fn deref_works() {
        let p = AbsolutePathBuf::try_from_str("/a/b").unwrap();
        let _: &Path = &*p;
        assert!(p.is_absolute());
    }

    #[test]
    fn into_path_buf_round_trips() {
        let p = AbsolutePathBuf::try_from_str("/x/y").unwrap();
        let buf = p.into_path_buf();
        assert_eq!(buf, PathBuf::from("/x/y"));
    }
}
