use serde::Serialize;
use sysinfo::System;

/// Snapshot of host system resource usage.
#[derive(Debug, Clone, Serialize)]
pub struct SystemResources {
    pub cpu_usage_pct: f32,
    pub memory_total_mb: u64,
    pub memory_used_mb: u64,
    pub memory_available_mb: u64,
    pub memory_usage_pct: f32,
    pub cpu_count: usize,
    pub hostname: String,
    pub os: String,
    pub uptime_secs: u64,
}

/// Collect current system resource usage via the `sysinfo` crate.
pub fn get_system_resources() -> SystemResources {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory() / (1024 * 1024);
    let used_mem = sys.used_memory() / (1024 * 1024);
    let available_mem = sys.available_memory() / (1024 * 1024);
    let mem_pct = if total_mem > 0 {
        (used_mem as f32 / total_mem as f32) * 100.0
    } else {
        0.0
    };

    SystemResources {
        cpu_usage_pct: sys.global_cpu_usage(),
        memory_total_mb: total_mem,
        memory_used_mb: used_mem,
        memory_available_mb: available_mem,
        memory_usage_pct: mem_pct,
        cpu_count: sys.cpus().len(),
        hostname: System::host_name().unwrap_or_default(),
        os: System::long_os_version().unwrap_or_default(),
        uptime_secs: System::uptime(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_system_resources_returns_nonzero() {
        let res = get_system_resources();
        assert!(res.memory_total_mb > 0, "memory_total_mb should be > 0");
        assert!(res.cpu_count > 0, "cpu_count should be > 0");
        assert!(res.uptime_secs > 0, "uptime_secs should be > 0");
    }

    #[test]
    fn test_system_resources_memory_consistency() {
        let res = get_system_resources();
        // used + available should be roughly equal to total (within rounding)
        assert!(
            res.memory_used_mb <= res.memory_total_mb,
            "used ({}) should be <= total ({})",
            res.memory_used_mb,
            res.memory_total_mb
        );
    }

    #[test]
    fn test_system_resources_serializes() {
        let res = get_system_resources();
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains("cpu_usage_pct"));
        assert!(json.contains("memory_total_mb"));
        assert!(json.contains("hostname"));
    }

    #[test]
    fn test_hostname_not_empty() {
        let res = get_system_resources();
        // On most systems hostname should be set
        assert!(!res.hostname.is_empty(), "hostname should not be empty");
    }
}
