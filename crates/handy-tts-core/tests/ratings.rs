use handy_tts_core::device::{Device, Ratings};
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
