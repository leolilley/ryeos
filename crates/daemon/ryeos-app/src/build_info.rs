use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub revision: &'static str,
    pub build_date: &'static str,
}

pub fn get() -> BuildInfo {
    get_for_version(env!("CARGO_PKG_VERSION"))
}

pub fn get_for_version(version: &'static str) -> BuildInfo {
    BuildInfo {
        version,
        revision: env!("RYEOS_VCS_REF"),
        build_date: env!("RYEOS_BUILD_DATE"),
    }
}
