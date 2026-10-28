//! Union of recent human-attention intervals, shared by every window/vehicle.
//! Bounds and identity barriers deliberately prefer undercounting to replay.

#[derive(Debug, Default)]
pub(crate) struct EngagedSimUsage {
    floor: u64,
    intervals: Vec<(u64, u64)>,
}

impl EngagedSimUsage {
    pub(crate) fn reset(&mut self, now_ms: u64) {
        self.floor = self.floor.max(now_ms);
        self.intervals.clear();
    }

    pub(crate) fn accept(&mut self, start: u64, end: u64, now_ms: u64) -> u32 {
        self.floor = self.floor.max(now_ms.saturating_sub(120_000));
        self.intervals.retain(|&(_, end)| end > self.floor);
        let start = start.max(self.floor);
        let end = end.min(now_ms);
        if start >= end {
            return 0;
        }
        let mut fresh = end - start;
        for &(a, b) in &self.intervals {
            fresh -= end.min(b).saturating_sub(start.max(a));
        }
        self.intervals.push((start, end));
        self.intervals.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(a, b) in &self.intervals {
            if let Some(last) = merged.last_mut()
                && a <= last.1
            {
                last.1 = last.1.max(b);
            } else {
                merged.push((a, b));
            }
        }
        // Even adversarially fragmented reports cannot grow this indefinitely.
        if merged.len() > 256 {
            self.floor = self.floor.max(merged[0].1);
            merged.remove(0);
        }
        self.intervals = merged;
        u32::try_from(fresh).unwrap_or(60_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_vehicles_retries_and_out_of_order_reports_count_the_union() {
        let mut usage = EngagedSimUsage::default();
        assert_eq!(usage.accept(30_000, 60_000, 60_000), 30_000);
        assert_eq!(usage.accept(30_000, 60_000, 60_000), 0);
        assert_eq!(usage.accept(45_000, 75_000, 75_000), 15_000);
        assert_eq!(usage.accept(10_000, 40_000, 75_000), 20_000);
        assert_eq!(usage.accept(20_000, 70_000, 75_000), 0);
    }

    #[test]
    fn idle_gaps_are_not_filled_and_account_boundaries_reject_old_intervals() {
        let mut usage = EngagedSimUsage::default();
        assert_eq!(usage.accept(10_000, 20_000, 20_000), 10_000);
        assert_eq!(usage.accept(40_000, 50_000, 50_000), 10_000);
        assert_eq!(usage.accept(20_000, 40_000, 50_000), 20_000);
        usage.reset(60_000);
        assert_eq!(usage.accept(10_000, 50_000, 65_000), 0);
        assert_eq!(usage.accept(50_000, 65_000, 65_000), 5_000);
        assert_eq!(usage.accept(10_000, 50_000, 300_000), 0);
    }
}
