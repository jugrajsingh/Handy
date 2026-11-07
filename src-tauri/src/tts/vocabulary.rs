//! The user's vocabulary file for technical reading: `<app data dir>/tts-vocabulary.json`.
//!
//! The file is read at the start of every read, so an edit takes effect on the next read
//! without a restart. A file that cannot be used never fails a read: the built-in
//! vocabulary is used and one notice is raised per distinct error.

use handy_tts_core::vocabulary::{Vocabulary, VocabularyError};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
};

/// The user file's name inside the app data dir.
pub const FILE_NAME: &str = "tts-vocabulary.json";

/// A new file holds no entries.
const NEW_FILE: &str = "{ \"version\": 1 }\n";

/// Why the user file was not used.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum FileError {
    #[error("the file cannot be read: {0}")]
    Unreadable(String),
    #[error("the file is not valid UTF-8 text (first bad byte at offset {0})")]
    NotUtf8(usize),
    #[error("{0}")]
    Invalid(VocabularyError),
}

/// The errors already reported in this app run, by text.
#[derive(Default)]
pub struct Reported(Mutex<BTreeSet<String>>);

impl Reported {
    /// True the first time `text` is seen.
    fn first(&self, text: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(text.to_owned())
    }
}

/// Loads the user file from `dir` (creating it when it is missing) and merges it over the
/// built-in vocabulary. When the file cannot be used, returns the built-in vocabulary and
/// calls `notify` once per distinct error text over the life of `reported`.
pub fn load_vocabulary(
    dir: &Path,
    reported: &Reported,
    notify: &mut dyn FnMut(&FileError),
) -> Arc<Vocabulary> {
    let path = dir.join(FILE_NAME);
    match read_user_file(dir, &path) {
        Ok(Some(file)) => Arc::new(Vocabulary::merged(&file)),
        Ok(None) => Arc::new(Vocabulary::builtin()),
        Err(error) => {
            let text = error.to_string();
            if reported.first(&text) {
                log::warn!(
                    "Read-aloud vocabulary {} not used, reading with the built-in vocabulary: {text}",
                    path.display()
                );
                notify(&error);
            }
            Arc::new(Vocabulary::builtin())
        }
    }
}

fn read_user_file(
    dir: &Path,
    path: &Path,
) -> Result<Option<handy_tts_core::vocabulary::VocabularyFile>, FileError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) =
                std::fs::create_dir_all(dir).and_then(|()| std::fs::write(path, NEW_FILE))
            {
                log::warn!(
                    "Read-aloud vocabulary {} could not be created: {error}",
                    path.display()
                );
            }
            return Ok(None);
        }
        Err(error) => return Err(FileError::Unreadable(error.to_string())),
    };
    let text = String::from_utf8(bytes)
        .map_err(|error| FileError::NotUtf8(error.utf8_error().valid_up_to()))?;
    Vocabulary::parse(&text)
        .map(Some)
        .map_err(FileError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::var_os("HANDY_TTS_TEST_SCRATCH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join("tts-vocabulary-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn load(dir: &Path, reported: &Reported) -> (Arc<Vocabulary>, Vec<FileError>) {
        let mut raised = Vec::new();
        let vocabulary = load_vocabulary(dir, reported, &mut |error| raised.push(error.clone()));
        (vocabulary, raised)
    }

    fn spoken(vocabulary: &Vocabulary, token: &str) -> String {
        handy_tts_core::vocabulary::speak_technical(
            token,
            vocabulary,
            &handy_tts_core::vocabulary::TechnicalOptions::default(),
        )
    }

    #[test]
    fn missing_user_file_is_created_and_builtin_used() {
        let dir = scratch("missing");
        let (vocabulary, raised) = load(&dir, &Reported::default());
        assert!(raised.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(),
            "{ \"version\": 1 }\n"
        );
        assert_eq!(*vocabulary, Vocabulary::builtin());
        assert_eq!(
            spoken(&vocabulary, "tests/lib.sh"),
            "tests slash lib dot S H"
        );
    }

    #[test]
    fn missing_data_dir_is_created_with_the_file() {
        let dir = scratch("missing-dir").join("nested").join("data");
        let (_, raised) = load(&dir, &Reported::default());
        assert!(raised.is_empty());
        assert!(dir.join(FILE_NAME).is_file());
    }

    #[test]
    fn user_entry_overrides_builtin() {
        let dir = scratch("override");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{ "version": 1, "extensions": { "sh": "shell" }, "terms": { "widgetron": "widget tron" } }"#,
        )
        .unwrap();
        let (vocabulary, raised) = load(&dir, &Reported::default());
        assert!(raised.is_empty());
        assert_eq!(spoken(&vocabulary, "lib.sh"), "lib dot shell");
        assert_eq!(spoken(&vocabulary, "widgetron"), "widget tron");
        assert_eq!(
            spoken(&Vocabulary::builtin(), "lib.sh"),
            "lib dot S H",
            "the built-in vocabulary is untouched"
        );
    }

    #[test]
    fn edit_takes_effect_on_next_load() {
        let dir = scratch("edit");
        let reported = Reported::default();
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{ "version": 1, "terms": { "widgetron": "widget tron" } }"#,
        )
        .unwrap();
        let (first, _) = load(&dir, &reported);
        assert_eq!(spoken(&first, "widgetron"), "widget tron");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{ "version": 1, "terms": { "widgetron": "wig it ron" } }"#,
        )
        .unwrap();
        let (second, raised) = load(&dir, &reported);
        assert!(raised.is_empty());
        assert_eq!(spoken(&second, "widgetron"), "wig it ron");
        assert_eq!(spoken(&first, "widgetron"), "widget tron");
    }

    mod invalid_user_vocabulary_falls_back_with_one_notice {
        use super::*;

        fn assert_one_notice(name: &str, install: impl Fn(&Path), expect: impl Fn(&FileError)) {
            let dir = scratch(name);
            install(&dir);
            let reported = Reported::default();
            let (vocabulary, raised) = load(&dir, &reported);
            assert_eq!(raised.len(), 1, "{name}: {raised:?}");
            expect(&raised[0]);
            assert_eq!(*vocabulary, Vocabulary::builtin(), "{name}");
            let (again, raised) = load(&dir, &reported);
            assert!(raised.is_empty(), "{name}: repeated: {raised:?}");
            assert_eq!(*again, Vocabulary::builtin(), "{name}");
            assert_eq!(spoken(&again, "tests/lib.sh"), "tests slash lib dot S H");
        }

        #[test]
        fn wrong_type() {
            assert_one_notice(
                "wrong-type",
                |dir| {
                    std::fs::write(
                        dir.join(FILE_NAME),
                        "{ \"version\": 1,\n  \"extensions\": { \"sh\": 7 } }",
                    )
                    .unwrap();
                },
                |error| match error {
                    FileError::Invalid(VocabularyError::Json { line, column, .. }) => {
                        assert_eq!(*line, 2, "{error}");
                        assert!(*column > 0, "{error}");
                        assert!(error.to_string().contains("line 2 column"), "{error}");
                    }
                    other => panic!("expected a JSON error with a position, got {other:?}"),
                },
            );
        }

        #[test]
        fn unknown_key() {
            assert_one_notice(
                "unknown-key",
                |dir| {
                    std::fs::write(
                        dir.join(FILE_NAME),
                        r#"{ "version": 1, "extensionz": { "sh": "shell" } }"#,
                    )
                    .unwrap();
                },
                |error| assert!(matches!(error, FileError::Invalid(_)), "{error:?}"),
            );
        }

        #[test]
        fn invalid_utf8() {
            assert_one_notice(
                "utf8",
                |dir| {
                    std::fs::write(dir.join(FILE_NAME), b"{ \"version\": 1, \"x\": \"\xff\" }")
                        .unwrap()
                },
                |error| assert!(matches!(error, FileError::NotUtf8(_)), "{error:?}"),
            );
        }

        #[test]
        fn unreadable_file() {
            assert_one_notice(
                "unreadable",
                |dir| std::fs::create_dir(dir.join(FILE_NAME)).unwrap(),
                |error| assert!(matches!(error, FileError::Unreadable(_)), "{error:?}"),
            );
        }

        #[test]
        fn unsupported_version() {
            assert_one_notice(
                "version",
                |dir| std::fs::write(dir.join(FILE_NAME), r#"{ "version": 2 }"#).unwrap(),
                |error| {
                    assert_eq!(*error, FileError::Invalid(VocabularyError::Version(2)));
                },
            );
        }

        #[test]
        fn a_different_error_is_reported_again() {
            let dir = scratch("distinct");
            let reported = Reported::default();
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            assert_eq!(load(&dir, &reported).1.len(), 1);
            assert_eq!(load(&dir, &reported).1.len(), 0);
            std::fs::write(dir.join(FILE_NAME), r#"{ "version": 3 }"#).unwrap();
            assert_eq!(load(&dir, &reported).1.len(), 1);
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            assert_eq!(
                load(&dir, &reported).1.len(),
                0,
                "an error already reported this run stays quiet"
            );
        }

        #[test]
        fn a_fixed_file_is_used_again() {
            let dir = scratch("fixed");
            let reported = Reported::default();
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            let _ = load(&dir, &reported);
            std::fs::write(
                dir.join(FILE_NAME),
                r#"{ "version": 1, "extensions": { "sh": "shell" } }"#,
            )
            .unwrap();
            let (vocabulary, raised) = load(&dir, &reported);
            assert!(raised.is_empty());
            assert_eq!(spoken(&vocabulary, "lib.sh"), "lib dot shell");
        }
    }
}
