//! Payload contract tests — assert the exact JSON shape never silently drifts.

#[cfg(test)]
mod tests {
    use incentiveswift_api::delivery::payload::{DeliveryPayload, ContactPayload, CampaignPayload, QuestionAnswerPair};

    /// Serialize a known DeliveryPayload and assert the exact JSON keys and shape.
    #[test]
    fn test_payload_contract_shape() {
        let payload = DeliveryPayload::build(
            ContactPayload {
                first_name: Some("Marcus".to_string()),
                last_name: Some("Torres".to_string()),
                email: Some("marcus@torreselectric.com".to_string()),
                phone: Some("+13125550100".to_string()),
                business_name: Some("Torres Electric LLC".to_string()),
            },
            CampaignPayload {
                name: "Summer Giveaway".to_string(),
                campaign_type: "raffle".to_string(),
                tag_namespace: "Summer_Giveaway".to_string(),
            },
            "winner".to_string(),
            vec!["Summer_Giveaway_Winner".to_string()],
            Some(74),
            vec![
                QuestionAnswerPair {
                    question: "What's your biggest challenge right now?".to_string(),
                    answer: "Not enough leads".to_string(),
                },
                QuestionAnswerPair {
                    question: "How many leads do you get per month?".to_string(),
                    answer: "10-50".to_string(),
                },
            ],
            "550e8400-e29b-41d4-a716-446655440000".to_string(),
        );

        let json = serde_json::to_value(&payload).unwrap();

        // Assert top-level keys exist
        assert_eq!(json["event"], "entry.captured");
        assert!(json["captured_at"].is_string());

        // Assert contact structure
        assert_eq!(json["contact"]["first_name"], "Marcus");
        assert_eq!(json["contact"]["last_name"], "Torres");
        assert_eq!(json["contact"]["email"], "marcus@torreselectric.com");
        assert_eq!(json["contact"]["phone"], "+13125550100");
        assert_eq!(json["contact"]["business_name"], "Torres Electric LLC");

        // Assert campaign structure
        assert_eq!(json["campaign"]["name"], "Summer Giveaway");
        assert_eq!(json["campaign"]["type"], "raffle");
        assert_eq!(json["campaign"]["tag_namespace"], "Summer_Giveaway");

        // Assert outcome and tags
        assert_eq!(json["outcome"], "winner");
        assert_eq!(json["tags_applied"][0], "Summer_Giveaway_Winner");
        assert_eq!(json["score"], 74);

        // Assert Q&A structure
        assert_eq!(json["questions_and_answers"][0]["question"], "What's your biggest challenge right now?");
        assert_eq!(json["questions_and_answers"][0]["answer"], "Not enough leads");
        assert_eq!(json["questions_and_answers"][1]["question"], "How many leads do you get per month?");
        assert_eq!(json["questions_and_answers"][1]["answer"], "10-50");

        // Assert entry_id
        assert_eq!(json["entry_id"], "550e8400-e29b-41d4-a716-446655440000");

        // Assert no unexpected top-level keys
        let expected_keys = vec![
            "event", "contact", "campaign", "outcome", "tags_applied",
            "score", "questions_and_answers", "entry_id", "captured_at"
        ];
        let json_obj = json.as_object().unwrap();
        for key in json_obj.keys() {
            assert!(expected_keys.contains(&key.as_str()), "Unexpected key: {}", key);
        }
    }
}
