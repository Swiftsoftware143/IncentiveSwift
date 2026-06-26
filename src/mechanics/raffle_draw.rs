//! Seeded Fisher-Yates shuffle for raffle draws.
//!
//! Given the same seed and the same set of entries, always produces the same winner.
//! The random_seed is stored permanently in the database and never overwritten,
//! guaranteeing auditability.

use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

/// Perform a seeded Fisher-Yates shuffle and return the first (winning) entry.
///
/// # Arguments
/// * `entries` - A vector of entry IDs (strings) to shuffle
/// * `seed` - A u64 seed that makes the shuffle deterministic
///
/// # Returns
/// The winning entry ID, or None if the entries list is empty
pub fn seeded_fisher_yates(entries: &[String], seed: u64) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut rng = StdRng::seed_from_u64(seed);
    let mut indices: Vec<usize> = (0..entries.len()).collect();

    // Fisher-Yates shuffle: iterate from the end, swapping each element
    // with a randomly selected element from the unprocessed portion
    for i in (1..indices.len()).rev() {
        let j = rng.gen_range(0..=i);
        indices.swap(i, j);
    }

    // The first element after shuffling is the winner
    Some(entries[indices[0]].clone())
}

/// Generate a reproducible random seed. Can be called once when creating a
/// raffle draw, and the result should be stored permanently.
pub fn generate_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_seed_same_winner() {
        let entries: Vec<String> = (1..=100).map(|i| format!("entry-{}", i)).collect();
        let seed = 42;

        let winner1 = seeded_fisher_yates(&entries, seed);
        let winner2 = seeded_fisher_yates(&entries, seed);

        assert_eq!(winner1, winner2, "Same seed must produce same winner");
    }

    #[test]
    fn test_different_seed_different_winner() {
        let entries: Vec<String> = (1..=100).map(|i| format!("entry-{}", i)).collect();

        let winner1 = seeded_fisher_yates(&entries, 42);
        let winner2 = seeded_fisher_yates(&entries, 99);

        assert_ne!(winner1, winner2, "Different seeds should (likely) produce different winners");
    }

    #[test]
    fn test_empty_entries() {
        let entries: Vec<String> = vec![];
        let result = seeded_fisher_yates(&entries, 42);
        assert!(result.is_none(), "Empty entries should return None");
    }

    #[test]
    fn test_single_entry() {
        let entries = vec!["only-entry".to_string()];
        let result = seeded_fisher_yates(&entries, 42);
        assert_eq!(result, Some("only-entry".to_string()));
    }

    #[test]
    fn test_deterministic_shuffle() {
        // Verify that the entire shuffled order is deterministic
        let entries: Vec<String> = (1..=10).map(|i| format!("e{}", i)).collect();
        let seed = 100;

        let mut rng = StdRng::seed_from_u64(seed);
        let mut indices: Vec<usize> = (0..entries.len()).collect();
        for i in (1..indices.len()).rev() {
            let j = rng.gen_range(0..=i);
            indices.swap(i, j);
        }

        let expected_first = entries[indices[0]].clone();
        let result = seeded_fisher_yates(&entries, seed);

        assert_eq!(result, Some(expected_first));
    }
}
