//! The majority rule (`chain_rescue_plan.md`, decided 2026-11-23): a new
//! republic's threshold m of n seats must satisfy 2m > n, so any two
//! deciding groups share a seat. A founding and branch-choice gate only,
//! never a `verify_chain` rule: older republics keep verifying.

/// 2m > n.
pub fn threshold_is_majority(m: u8, n: u8) -> bool {
    2 * u16::from(m) > u16::from(n)
}

/// The smallest m with 2m > n.
pub fn min_majority(n: u8) -> u8 {
    n / 2 + 1
}

/// The founding refusal; it names the smallest allowed m.
pub fn majority_refusal(n: u8) -> String {
    format!("needs a majority: at least {} of {n}", min_majority(n))
}

/// The fewest seats a majority republic needs for the vault
/// (m <= n - 2 with 2m > n).
pub const VAULT_MIN_SEATS: u8 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majority_means_more_than_half() {
        for (m, n) in [(2, 2), (2, 3), (3, 4), (3, 5), (4, 6), (7, 13), (13, 13)] {
            assert!(threshold_is_majority(m, n), "{m}-of-{n}");
        }
        for (m, n) in [(1, 2), (2, 4), (2, 5), (3, 6), (6, 13), (0, 0), (1, 3)] {
            assert!(!threshold_is_majority(m, n), "{m}-of-{n}");
        }
        assert!(threshold_is_majority(255, 255), "no overflow");
    }

    #[test]
    fn the_smallest_majority_is_the_first_allowed_m() {
        for n in 1..=13u8 {
            let min = min_majority(n);
            assert!(threshold_is_majority(min, n));
            assert!(!threshold_is_majority(min - 1, n));
        }
        assert_eq!(majority_refusal(5), "needs a majority: at least 3 of 5");
    }

    #[test]
    fn the_vault_needs_five_seats_under_the_majority_rule() {
        let fits = |n: u8| min_majority(n) >= 2 && u16::from(min_majority(n)) + 2 <= u16::from(n);
        assert!(fits(VAULT_MIN_SEATS));
        assert!((2..VAULT_MIN_SEATS).all(|n| !fits(n)));
    }
}
