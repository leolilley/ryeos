use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub revision: &'static str,
    pub build_date: &'static str,
    pub profile: &'static str,
}

pub const RELEASE_PROFILE: &str = "release";
pub const LATENCY_PROFILING_PROFILE: &str = "latency-profiling";

pub const fn compiled_profile() -> &'static str {
    if cfg!(feature = "latency-profiling") {
        LATENCY_PROFILING_PROFILE
    } else {
        RELEASE_PROFILE
    }
}

pub fn get() -> BuildInfo {
    get_for_version(env!("CARGO_PKG_VERSION"))
}

pub fn get_for_version(version: &'static str) -> BuildInfo {
    BuildInfo {
        version,
        revision: env!("RYEOS_VCS_REF"),
        build_date: env!("RYEOS_BUILD_DATE"),
        profile: compiled_profile(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_reports_the_compiled_artifact_profile() {
        let expected = if cfg!(feature = "latency-profiling") {
            LATENCY_PROFILING_PROFILE
        } else {
            RELEASE_PROFILE
        };
        assert_eq!(get_for_version("test").profile, expected);
    }

    #[test]
    fn build_info_json_carries_the_artifact_profile() {
        let value = serde_json::to_value(get_for_version("test")).unwrap();
        assert_eq!(value["profile"], compiled_profile());
    }
}
