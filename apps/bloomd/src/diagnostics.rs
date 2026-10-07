use std::{env, sync::OnceLock};

static ENABLED: OnceLock<bool> = OnceLock::new();

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| env::var("BLOOM_TIMING_DIAGNOSTICS").as_deref() == Ok("1"))
}

pub fn elapsed_micros(start: std::time::Instant) -> u128 {
    start.elapsed().as_micros()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_time_is_monotonic_nonnegative() {
        let start = std::time::Instant::now();
        assert!(elapsed_micros(start) < 1_000_000);
    }
}
