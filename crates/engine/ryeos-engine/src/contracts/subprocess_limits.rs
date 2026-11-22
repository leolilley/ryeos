//! Signed restrictions on a native child, independent of its provider or purpose.

use serde::{Deserialize, Serialize};

/// These are restrictions requested by the composed subject. Node policy still
/// applies independently. CPU is per process and address space is virtual memory;
/// neither field attests aggregate allocation or observed physical resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSubprocessLimits {
    pub max_open_files: Option<u64>,
    pub max_address_space_bytes: Option<u64>,
    pub max_cpu_seconds: Option<u64>,
    /// OS account limit, not a private process-tree counter.
    pub max_processes: Option<u64>,
    pub deny_process_creation: bool,
    pub max_stdout_bytes: Option<u64>,
    pub max_stderr_bytes: Option<u64>,
}

impl ExecutionSubprocessLimits {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("max_open_files", self.max_open_files),
            ("max_address_space_bytes", self.max_address_space_bytes),
            ("max_cpu_seconds", self.max_cpu_seconds),
            ("max_processes", self.max_processes),
            ("max_stdout_bytes", self.max_stdout_bytes),
            ("max_stderr_bytes", self.max_stderr_bytes),
        ] {
            anyhow::ensure!(value != Some(0), "{name} must be positive when selected");
        }
        Ok(())
    }

    /// Preserve the native owner's exact units and enforcement semantics.
    pub fn to_native(self) -> anyhow::Result<lillux::SubprocessLimits> {
        self.validate()?;
        Ok(lillux::SubprocessLimits {
            max_open_files: self.max_open_files,
            max_address_space_bytes: self.max_address_space_bytes,
            max_cpu_seconds: self.max_cpu_seconds,
            max_processes: self.max_processes,
            deny_process_creation: self.deny_process_creation,
            max_stdout_bytes: self.max_stdout_bytes,
            max_stderr_bytes: self.max_stderr_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionSubprocessLimits;

    #[test]
    fn authored_limits_reject_unknown_fields_and_zero_before_native_launch() {
        let exact = serde_json::json!({
            "max_cpu_seconds": 3,
            "max_address_space_bytes": 134217728,
            "deny_process_creation": true,
            "max_stdout_bytes": 32768,
            "max_stderr_bytes": 8192
        });
        let limits: ExecutionSubprocessLimits = serde_json::from_value(exact.clone()).unwrap();
        let native = limits.to_native().unwrap();
        assert_eq!(native.max_cpu_seconds, Some(3));
        assert_eq!(native.max_address_space_bytes, Some(134217728));
        assert!(native.deny_process_creation);
        let mut zero = exact.clone();
        zero["max_cpu_seconds"] = serde_json::json!(0);
        assert!(
            serde_json::from_value::<ExecutionSubprocessLimits>(zero)
                .unwrap()
                .to_native()
                .is_err()
        );
        let mut unknown = exact;
        unknown["aggregate_cpu_seconds"] = serde_json::json!(3);
        assert!(serde_json::from_value::<ExecutionSubprocessLimits>(unknown).is_err());
    }
}
