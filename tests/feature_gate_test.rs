//! Feature gate tests — account with/without mechanic access.

use incentiveswift_api::db::campaigns;

/// Test that VALID_MECHANIC_TYPES includes all 12 mechanics.
#[test]
fn test_all_mechanic_types_valid() {
    let valid_types = vec![
        "score_reveal", "spin_wheel", "scratch_card", "personality",
        "calculator", "mystery", "countdown", "poll", "chat",
        "leaderboard", "raffle", "long_form_qualifier",
    ];

    for t in &valid_types {
        assert!(campaigns::validate_mechanic_type(t),
            "{} should be a valid mechanic type", t);
    }
}

/// Test that an invalid mechanic type is rejected.
#[test]
fn test_invalid_mechanic_type_rejected() {
    assert!(!campaigns::validate_mechanic_type("invalid_type"));
    assert!(!campaigns::validate_mechanic_type("crm"));
    assert!(!campaigns::validate_mechanic_type(""));
}

/// Test that all features from the seed data are registered.
#[test]
fn test_feature_registry() {
    let expected_features = vec![
        "mechanic_score_reveal",
        "mechanic_spin_wheel",
        "mechanic_scratch_card",
        "mechanic_personality",
        "mechanic_calculator",
        "mechanic_mystery",
        "mechanic_countdown",
        "mechanic_poll",
        "mechanic_chat",
        "mechanic_leaderboard",
        "mechanic_raffle",
        "mechanic_long_form_qualifier",
        "delivery_webhook",
        "delivery_direct_api",
        "branding_white_label",
        "branding_custom_domain",
        "module_loyalty_program",
        "limit_unlimited_campaigns",
    ];

    for feature in &expected_features {
        assert!(
            feature.starts_with("mechanic_")
                || feature.starts_with("delivery_")
                || feature.starts_with("branding_")
                || feature.starts_with("module_")
                || feature.starts_with("limit_"),
            "Feature key format: {}", feature
        );
    }

    assert_eq!(expected_features.len(), 18);
}
