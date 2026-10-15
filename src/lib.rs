pub mod config;
pub mod discovery;
pub mod gc;
pub mod platform;
pub mod storage;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Future timestamps must not suppress maintenance indefinitely after a clock change.
pub fn due(last: Option<u64>, interval: u64, now: u64) -> bool {
    match last {
        None => true,
        Some(last) if last > now.saturating_add(300) => true,
        Some(last) => now.saturating_sub(last) >= interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_handles_first_run_boundaries_and_clock_changes() {
        assert!(due(None, 86400, 1));
        assert!(!due(Some(100), 86400, 86499));
        assert!(due(Some(100), 86400, 86500));
        assert!(due(Some(100000), 86400, 100));
        assert!(!due(Some(101), 86400, 100));
    }
}
