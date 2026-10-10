//! Nearest-rank percentiles shared by the summary cost distributions and the
//! context-efficiency session distributions.

/// Nearest-rank percentile (`ceil(p * n) - 1`, clamped to the slice bounds)
/// of an ascending-sorted slice. Empty input yields `0.0`.
pub(crate) fn nearest_rank_percentile(sorted: &[f64], p: f64) -> f64 {
    nearest_rank_index(sorted.len(), p)
        .map(|rank| sorted[rank])
        .unwrap_or(0.0)
}

/// Index selected by the nearest-rank rule for a sorted slice of `len`
/// values, or `None` when the slice is empty.
pub(crate) fn nearest_rank_index(len: usize, p: f64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(((p * len as f64).ceil() as i64 - 1).clamp(0, len as i64 - 1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_index_follows_ceil_rule_and_clamps() {
        assert_eq!(nearest_rank_index(0, 0.5), None);
        assert_eq!(nearest_rank_index(1, 0.5), Some(0));
        assert_eq!(nearest_rank_index(2, 0.5), Some(0));
        assert_eq!(nearest_rank_index(2, 0.95), Some(1));
        assert_eq!(nearest_rank_index(10, 0.5), Some(4));
        assert_eq!(nearest_rank_index(10, 0.95), Some(9));
        assert_eq!(nearest_rank_index(20, 0.95), Some(18));
        assert_eq!(nearest_rank_index(4, 0.0), Some(0));
        assert_eq!(nearest_rank_index(4, 2.0), Some(3));
    }

    #[test]
    fn nearest_rank_percentile_reads_the_selected_value() {
        assert_eq!(nearest_rank_percentile(&[], 0.5), 0.0);
        assert_eq!(nearest_rank_percentile(&[7.5], 0.95), 7.5);
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(nearest_rank_percentile(&sorted, 0.5), 2.0);
        assert_eq!(nearest_rank_percentile(&sorted, 0.95), 4.0);
    }
}
