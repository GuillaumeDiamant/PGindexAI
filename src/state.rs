//! Runtime state of the daemon (independent from the static configuration)

/// Daemon state:
/// - `Healthy`  : pg_stat_statements active, all detections available
/// - `Degraded` : pg_stat_statements missing → missing-index detection
///   is DISABLED (no data source), the rest still works
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Healthy,
    Degraded,
}

impl DaemonState {
    pub fn is_degraded(self) -> bool {
        matches!(self, DaemonState::Degraded)
    }
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonState::Healthy => write!(f, "HEALTHY"),
            DaemonState::Degraded => write!(f, "DEGRADED"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_degraded_flag() {
        assert!(DaemonState::Degraded.is_degraded());
        assert!(!DaemonState::Healthy.is_degraded());
    }

    #[test]
    fn test_display() {
        assert_eq!(DaemonState::Healthy.to_string(), "HEALTHY");
        assert_eq!(DaemonState::Degraded.to_string(), "DEGRADED");
    }
}
