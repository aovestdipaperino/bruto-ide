//! Per-line profile data as shown by the editor's heat column.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Width of the profile column drawn between the gutter and the text.
pub const PROFILE_COL_WIDTH: i16 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heat {
    /// Below 1 percent or never executed: no tint.
    None,
    /// 1 to 5 percent of total time.
    Cold,
    /// 5 to 20 percent.
    Warm,
    /// Above 20 percent.
    Hot,
}

/// Profile of one buffer: line -> (self_ns, hits), plus what it takes to
/// know when the data went stale.
#[derive(Debug, Clone)]
pub struct LineProfile {
    pub lines: HashMap<usize, (u64, u64)>,
    pub total_ns: u64,
    pub visible: bool,
    /// `text_hash` of the buffer when the profile was taken; a differing
    /// hash means the user edited and the column must go.
    pub text_hash: u64,
}

pub fn heat_bucket(share: f64) -> Heat {
    if share >= 0.20 {
        Heat::Hot
    } else if share >= 0.05 {
        Heat::Warm
    } else if share >= 0.01 {
        Heat::Cold
    } else {
        Heat::None
    }
}

/// The column text for a line: `" 12.4% "`, `"100.0% "`, `" <0.1% "`, or
/// blanks when the line never ran (or there is no total).
pub fn format_share(self_ns: u64, total_ns: u64) -> String {
    if total_ns == 0 || self_ns == 0 {
        return " ".repeat(PROFILE_COL_WIDTH as usize);
    }
    let pct = self_ns as f64 * 100.0 / total_ns as f64;
    if pct < 0.1 {
        return " <0.1% ".to_string();
    }
    format!("{pct:5.1}% ")
}

pub fn text_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_follow_the_spec_thresholds() {
        assert_eq!(heat_bucket(0.0), Heat::None);
        assert_eq!(heat_bucket(0.009), Heat::None);
        assert_eq!(heat_bucket(0.01), Heat::Cold);
        assert_eq!(heat_bucket(0.049), Heat::Cold);
        assert_eq!(heat_bucket(0.05), Heat::Warm);
        assert_eq!(heat_bucket(0.199), Heat::Warm);
        assert_eq!(heat_bucket(0.2), Heat::Hot);
        assert_eq!(heat_bucket(1.0), Heat::Hot);
    }

    #[test]
    fn share_is_always_seven_chars() {
        assert_eq!(format_share(124, 1000), " 12.4% ");
        assert_eq!(format_share(1000, 1000), "100.0% ");
        assert_eq!(format_share(1, 100_000), " <0.1% ");
        assert_eq!(format_share(0, 1000), "       ");
        assert_eq!(format_share(5, 0), "       ");
        for s in [
            format_share(124, 1000),
            format_share(1, 100_000),
            format_share(0, 1),
        ] {
            assert_eq!(s.chars().count(), PROFILE_COL_WIDTH as usize, "{s:?}");
        }
    }

    #[test]
    fn text_hash_changes_with_text() {
        assert_eq!(text_hash("a"), text_hash("a"));
        assert_ne!(text_hash("a"), text_hash("b"));
    }
}
