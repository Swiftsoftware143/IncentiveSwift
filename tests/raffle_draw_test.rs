//! Raffle draw tests — seeded Fisher-Yates determinism.

use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;

/// Seeded Fisher-Yates shuffle implementation (inline for tests).
fn seeded_fisher_yates(entries: &[String], seed: u64) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut rng = StdRng::seed_from_u64(seed);
    let mut indices: Vec<usize> = (0..entries.len()).collect();

    for i in (1..indices.len()).rev() {
        let j = rng.gen_range(0..=i);
        indices.swap(i, j);
    }

    Some(entries[indices[0]].clone())
}

/// Return the full shuffled order for testing.
#[allow(dead_code)]
fn fisher_yates_full(entries: &[String], seed: u64) -> Vec<String> {
    if entries.is_empty() {
        return vec![];
    }

    let mut rng = StdRng::seed_from_u64(seed);
    let mut result = entries.to_vec();

    for i in (1..result.len()).rev() {
        let j = rng.gen_range(0..=i);
        result.swap(i, j);
    }

    result
}

/// Test that seeded_fisher_yates produces the same winner given the same seed + same entries.
#[test]
fn test_same_seed_same_winner() {
    let entries: Vec<String> = (1..=100).map(|i| format!("entry-{}", i)).collect();
    let seed = 42;

    let winner1 = seeded_fisher_yates(&entries, seed);
    let winner2 = seeded_fisher_yates(&entries, seed);
    assert_eq!(winner1, winner2, "Same seed + same entries = same winner");
}

/// Test that different seeds produce different winners.
#[test]
fn test_different_seed_different_winner() {
    let entries: Vec<String> = (1..=100).map(|i| format!("entry-{}", i)).collect();
    let winner1 = seeded_fisher_yates(&entries, 42);
    let winner2 = seeded_fisher_yates(&entries, 99);
    assert_ne!(winner1, winner2, "Different seeds should produce different winners");
}

/// Test empty entries returns None.
#[test]
fn test_empty_entries() {
    let entries: Vec<String> = vec![];
    let result = seeded_fisher_yates(&entries, 42);
    assert!(result.is_none());
}

/// Test single entry always wins.
#[test]
fn test_single_entry() {
    let entries = vec!["only-entry".to_string()];
    let result = seeded_fisher_yates(&entries, 42);
    assert_eq!(result, Some("only-entry".to_string()));
}

/// Test the full shuffle is deterministic, not just the winner.
#[test]
fn test_full_shuffle_deterministic() {
    let entries: Vec<String> = (1..=10).map(|i| format!("e{}", i)).collect();
    let seed = 100;

    let shuffled1 = fisher_yates_full(&entries, seed);
    let shuffled2 = fisher_yates_full(&entries, seed);

    assert_eq!(shuffled1, shuffled2, "Full shuffle must be deterministic");
}
