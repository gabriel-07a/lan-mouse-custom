//! Small assertion helpers for behaviour tests.
//!
//! These are intentionally tiny: they make intent explicit in table-driven
//! tests without introducing snapshot frameworks or sleeps.

use std::fmt::Debug;

/// Assert two floats are within `epsilon`.
#[track_caller]
pub fn assert_close(actual: f32, expected: f32, epsilon: f32) {
    assert!(
        (actual - expected).abs() <= epsilon,
        "expected {expected} ± {epsilon}, got {actual}"
    );
}

/// Assert a value is one of `allowed` (useful for state-machine outcomes where
/// order between events is not deterministic but the set is fixed).
#[track_caller]
pub fn assert_one_of<T: PartialEq + Debug>(actual: &T, allowed: &[T]) {
    assert!(
        allowed.iter().any(|candidate| candidate == actual),
        "got {actual:?}, expected one of {allowed:?}"
    );
}

/// Assert two slices are element-wise equal, with an index in the failure.
#[track_caller]
pub fn assert_sequence<T: PartialEq + Debug>(actual: &[T], expected: &[T]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "sequence length differs: got {actual:?}, expected {expected:?}"
    );
    for (index, (got, want)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(got, want, "sequence differs at index {index}");
    }
}

/// Assert every needle appears in `haystack`.
#[track_caller]
pub fn assert_contains_all(haystack: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            haystack.contains(needle),
            "missing {needle:?} in {haystack:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_is_inclusive() {
        assert_close(1.0, 1.05, 0.05);
    }

    #[test]
    #[should_panic(expected = "expected")]
    fn close_rejects_a_larger_difference() {
        assert_close(1.0, 1.2, 0.05);
    }

    #[test]
    fn sequence_reports_the_index() {
        assert_sequence(&[1, 2, 3], &[1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "index 2")]
    fn sequence_fails_at_the_first_difference() {
        assert_sequence(&[1, 2, 4], &[1, 2, 3]);
    }

    #[test]
    fn one_of_accepts_members() {
        assert_one_of(&2, &[1, 2, 3]);
    }
}
