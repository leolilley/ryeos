use std::env;
use std::fs;
use std::path::PathBuf;

const EXACT_BUILD_ENV: &str = "RYEOS_EXACT_BUILD_PROVENANCE";
const BUILD_VERSION_ENV: &str = "RYEOS_BUILD_VERSION";
const VCS_REF_ENV: &str = "RYEOS_VCS_REF";
const BUILD_DATE_ENV: &str = "RYEOS_BUILD_DATE";
const SOURCE_DATE_EPOCH_ENV: &str = "SOURCE_DATE_EPOCH";

fn main() {
    for name in [
        EXACT_BUILD_ENV,
        BUILD_VERSION_ENV,
        VCS_REF_ENV,
        BUILD_DATE_ENV,
        SOURCE_DATE_EPOCH_ENV,
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let target = required_cargo_value("TARGET");
    let profile = required_cargo_value("PROFILE");
    let exact = match env::var(EXACT_BUILD_ENV) {
        Ok(value) if value == "1" => true,
        Ok(value) => panic!("{EXACT_BUILD_ENV} must be exactly `1` when present, got `{value}`"),
        Err(env::VarError::NotPresent) => false,
        Err(error) => panic!("read {EXACT_BUILD_ENV}: {error}"),
    };

    let (qualification, version, revision, build_date, source_date_epoch) = if exact {
        if profile != "release" {
            panic!("an exact workload-client realization requires Cargo profile `release`");
        }
        (
            "exact".to_owned(),
            required_exact_value(BUILD_VERSION_ENV),
            required_exact_value(VCS_REF_ENV),
            required_exact_value(BUILD_DATE_ENV),
            required_exact_value(SOURCE_DATE_EPOCH_ENV),
        )
    } else {
        (
            "development".to_owned(),
            required_cargo_value("CARGO_PKG_VERSION"),
            "unqualified".to_owned(),
            "unqualified".to_owned(),
            "unqualified".to_owned(),
        )
    };

    let testimony = format!(
        concat!(
            "schema=ryeos.workload-client-build.v1\n",
            "qualification={}\n",
            "version={}\n",
            "source_revision={}\n",
            "build_date={}\n",
            "source_date_epoch={}\n",
            "target={}\n",
            "profile={}\n",
        ),
        qualification, version, revision, build_date, source_date_epoch, target, profile,
    );
    let out_dir = PathBuf::from(required_cargo_value("OUT_DIR"));
    fs::write(out_dir.join("ryeos-workload-client-build"), testimony)
        .expect("write workload-client build testimony");
}

fn required_cargo_value(name: &str) -> String {
    env::var(name)
        .unwrap_or_else(|error| panic!("read Cargo-provided {name}: {error}"))
        .trim()
        .to_owned()
}

fn required_exact_value(name: &str) -> String {
    let value = env::var(name).unwrap_or_else(|error| {
        panic!("exact workload-client build requires explicit {name}: {error}")
    });
    if value.is_empty()
        || value == "unknown"
        || value == "unqualified"
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        panic!("exact workload-client build received non-canonical {name}");
    }
    value
}
