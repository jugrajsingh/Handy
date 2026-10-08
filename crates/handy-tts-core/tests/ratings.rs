use handy_tts_core::device::{Device, Ratings};

#[test]
fn optional_lane_fields_validate_and_default_to_one_lane() {
    let original: serde_json::Value = serde_json::from_str(include_str!(
        "../../../src-tauri/resources/tts/ratings.json"
    ))
    .unwrap();
    let ids = ["pocket-en", "supertonic-3", "kokoro-82m-int8"];
    let mut legacy = original.clone();
    let row = &mut legacy["models"]["pocket-en"]["devices"]["cpu"];
    for field in ["best_lanes", "rtf_by_lanes", "base_rss_mib", "lane_rss_mib"] {
        row.as_object_mut().unwrap().remove(field);
    }
    let ratings: Ratings = serde_json::from_value(legacy.clone()).unwrap();
    ratings.validate(&ids).unwrap();
    assert_eq!(
        ratings.models["pocket-en"].devices[&Device::Cpu]
            .lanes
            .best_lanes,
        1
    );
    for (field, value) in [
        ("best_lanes", serde_json::json!(0)),
        ("best_lanes", serde_json::json!(4)),
        ("rtf_by_lanes", serde_json::json!([0.1, -0.2, 0.3])),
        ("base_rss_mib", serde_json::json!(-1)),
        ("lane_rss_mib", serde_json::json!(-1)),
    ] {
        let mut invalid = legacy.clone();
        invalid["models"]["pocket-en"]["devices"]["cpu"][field] = value;
        let ratings: Ratings = serde_json::from_value(invalid).unwrap();
        assert!(ratings.validate(&ids).is_err(), "{field}");
    }
    let mut measured = legacy;
    let row = &mut measured["models"]["pocket-en"]["devices"]["cpu"];
    row["best_lanes"] = serde_json::json!(2);
    row["rtf_by_lanes"] = serde_json::json!([0.3, 0.2, 0.25]);
    row["base_rss_mib"] = serde_json::json!(20.0);
    row["lane_rss_mib"] = serde_json::json!(400.0);
    let ratings: Ratings = serde_json::from_value(measured).unwrap();
    ratings.validate(&ids).unwrap();
    let serialized = serde_json::to_value(ratings).unwrap();
    assert_eq!(
        serialized["models"]["pocket-en"]["devices"]["cpu"]["rtf_by_lanes"],
        serde_json::json!([0.3, 0.2, 0.25])
    );
}
#[test]
fn shipped_ratings_cover_exact_registry_and_validate_schema() {
    let ratings: Ratings = serde_json::from_str(include_str!(
        "../../../src-tauri/resources/tts/ratings.json"
    ))
    .unwrap();
    let entries = handy_tts_core::catalog::entries().unwrap();
    let ids = entries.iter().map(|entry| entry.id).collect::<Vec<_>>();
    ratings.validate(&ids).unwrap();
    assert_eq!(ratings.models["pocket-en"].quality, 9);
    assert_eq!(ratings.models["supertonic-3"].quality, 8);
    assert_eq!(ratings.models["kokoro-82m-int8"].quality, 6);
    assert!(
        ratings.models["kokoro-82m-int8"].devices[&Device::Gpu]
            .rtf_2x
            .is_none()
    );
}
