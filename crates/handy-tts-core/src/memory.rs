pub const MIN_AVAILABLE_BYTES: u64 = 1024 * 1024 * 1024;
pub const LANE_RESERVE_BYTES: u64 = 1536 * 1024 * 1024;

/// Sizes synthesis concurrency from the measured resident memory and free-memory reserve.
pub fn lanes_for(mem_available: u64, base_rss: u64, lane_rss: u64, best_lanes: usize) -> usize {
    if lane_rss == 0 {
        return 1;
    }
    let ram_lanes = mem_available
        .saturating_sub(LANE_RESERVE_BYTES)
        .saturating_sub(base_rss)
        / lane_rss;
    ram_lanes.min(best_lanes.clamp(1, 3) as u64).clamp(1, 3) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_for_respects_reserve_best_and_floor() {
        let mib = 1024 * 1024;
        for (mem, base, lane, best, expected) in [
            (1434, 200, 100, 3, 1),
            (1536, 0, 100, 3, 1),
            (1836, 100, 100, 3, 2),
            (1936, 100, 100, 3, 3),
            (1935, 100, 100, 3, 2),
            (4096, 100, 100, 2, 2),
            (4096, 100, 100, 8, 3),
            (4096, 100, 100, 0, 1),
            (0, 200, 100, 3, 1),
            (4096, 100, 0, 3, 1),
        ] {
            assert_eq!(
                lanes_for(mem * mib, base * mib, lane * mib, best),
                expected,
                "mem={mem}, base={base}, lane={lane}, best={best}"
            );
        }
        assert_eq!(LANE_RESERVE_BYTES, 1536 * mib);
        assert_eq!(lanes_for(u64::MAX, 0, 1, 3), 3);
    }
}
