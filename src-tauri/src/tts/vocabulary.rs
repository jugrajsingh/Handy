//! The user's vocabulary file for technical reading: `<app data dir>/tts-vocabulary.json`.
//!
//! The file is read at the start of every read, so an edit takes effect on the next read
//! without a restart. A file that cannot be used never fails a read and never blocks one: the
//! built-in vocabulary is used and one warning is raised per distinct error. The next
//! successful load ends that warning.

use crate::notices::{Notice, NoticeCode, NoticeSource};
use handy_tts_core::vocabulary::{Vocabulary, VocabularyError};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The user file's name inside the app data dir.
pub const FILE_NAME: &str = "tts-vocabulary.json";

/// A new file holds no entries.
const NEW_FILE: &str = "{ \"version\": 1 }\n";

/// The largest vocabulary file that is read.
const MAX_BYTES: u64 = 1 << 20;

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

/// What a load found, for whoever shows warnings.
pub enum Outcome<'a> {
    /// The file cannot be used and this error has not been reported in this run.
    Failed(&'a FileError),
    /// The file is usable, or absent and now created.
    Usable,
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

    /// Forgets every error, so a file that breaks again is reported again.
    fn clear(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// Loads the user file from `dir` (creating it when it is missing) and merges it over the
/// built-in vocabulary. When the file cannot be used, returns the built-in vocabulary and
/// calls `notify` once per distinct error text until a usable file is seen, which calls it
/// with [`Outcome::Usable`] and forgets the errors.
pub fn load_vocabulary(
    dir: &Path,
    reported: &Reported,
    notify: &mut dyn FnMut(Outcome<'_>),
) -> Arc<Vocabulary> {
    let path = dir.join(FILE_NAME);
    match read_user_file(dir, &path) {
        Ok(Some(file)) => {
            reported.clear();
            notify(Outcome::Usable);
            Arc::new(Vocabulary::merged(&file))
        }
        Ok(None) => {
            reported.clear();
            notify(Outcome::Usable);
            Arc::new(Vocabulary::builtin())
        }
        Err(error) => {
            let text = error.to_string();
            if reported.first(&text) {
                log::warn!(
                    "Read-aloud vocabulary {} not used, reading with the built-in vocabulary: {text}",
                    path.display()
                );
                notify(Outcome::Failed(&error));
            }
            Arc::new(Vocabulary::builtin())
        }
    }
}

/// Refuses anything that is not a regular file; a FIFO or device would block or never end.
fn ensure_regular(metadata: &std::fs::Metadata) -> Result<(), FileError> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(FileError::Unreadable("it is not a regular file".into()))
    }
}

/// Opens the file without ever blocking: the type is checked before the open, and the open
/// itself does not wait for a writer, so a path swapped for a FIFO in between is refused by
/// the check on the opened handle.
fn open_regular(path: &Path) -> Result<std::fs::File, FileError> {
    let unreadable = |error: std::io::Error| FileError::Unreadable(error.to_string());
    ensure_regular(&std::fs::metadata(path).map_err(unreadable)?)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(unreadable)?;
    ensure_regular(&file.metadata().map_err(unreadable)?)?;
    Ok(file)
}

/// Creates the file with no entries; a file created by someone else in the meantime is kept.
fn create_user_file(dir: &Path, path: &Path) {
    use std::io::Write;
    let created = std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut file| file.write_all(NEW_FILE.as_bytes()))
    });
    match created {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => log::warn!(
            "Read-aloud vocabulary {} could not be created: {error}",
            path.display()
        ),
        _ => {}
    }
}

fn read_user_file(
    dir: &Path,
    path: &Path,
) -> Result<Option<handy_tts_core::vocabulary::VocabularyFile>, FileError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_user_file(dir, path);
            return Ok(None);
        }
        Err(error) => return Err(FileError::Unreadable(error.to_string())),
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    open_regular(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| FileError::Unreadable(error.to_string()))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(FileError::Unreadable("file larger than 1 MiB".into()));
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| FileError::NotUtf8(error.utf8_error().valid_up_to()))?;
    Vocabulary::parse(&text)
        .map(Some)
        .map_err(FileError::Invalid)
}

/// The warning for an unusable file; `detail` names the file and the error with its position.
pub fn warning_notice(path: &Path, error: &FileError) -> Notice {
    Notice::new(NoticeCode::VocabularyInvalid, NoticeSource::ReadAloud)
        .with_detail(format!("{}: {error}", path.display()))
}

/// What the loader needs from the app: where the file lives and how to show or end a warning.
pub trait Host {
    /// The directory holding the user file, when it can be resolved.
    fn dir(&self) -> Option<PathBuf>;
    fn raise(&self, notice: Notice);
    /// Ends the vocabulary warning only; other Read aloud notices stay.
    fn clear(&self);
}

/// The vocabulary for each document: the built-in one plus the user's file, read again every
/// time, with its warning following the notice lifecycle.
pub struct UserVocabulary<H: Host> {
    host: H,
    reported: Reported,
}

impl<H: Host> UserVocabulary<H> {
    pub fn new(host: H) -> Self {
        Self {
            host,
            reported: Reported::default(),
        }
    }

    pub fn load(&self) -> Arc<Vocabulary> {
        let Some(dir) = self.host.dir() else {
            log::warn!("Read-aloud vocabulary: no app data dir, using the built-in one");
            return Arc::new(Vocabulary::builtin());
        };
        let path = dir.join(FILE_NAME);
        load_vocabulary(&dir, &self.reported, &mut |outcome| match outcome {
            Outcome::Failed(error) => self.host.raise(warning_notice(&path, error)),
            Outcome::Usable => self.host.clear(),
        })
    }

    #[cfg(test)]
    pub(crate) fn host(&self) -> &H {
        &self.host
    }

    /// Call when Read aloud notices are cleared, so a file that is still broken warns again.
    pub fn forget(&self) {
        self.reported.clear();
    }
}

/// The production host: the app data dir and the notice store.
pub struct AppHost(pub tauri::AppHandle);

impl Host for AppHost {
    fn dir(&self) -> Option<PathBuf> {
        crate::portable::app_data_dir(&self.0).ok()
    }

    fn raise(&self, notice: Notice) {
        crate::notices::raise(&self.0, notice);
    }

    fn clear(&self) {
        crate::notices::clear_matching(
            &self.0,
            NoticeSource::ReadAloud,
            Some(NoticeCode::VocabularyInvalid),
        );
    }
}

/// Test host for this module and its callers: a scratch dir and a notice list that follows
/// the store's one-notice-per-source rule.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::notices::{insert, remove};

    pub fn scratch(name: &str) -> PathBuf {
        let root = std::env::var_os("HANDY_TTS_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join("tts-vocabulary-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub struct LogHost {
        dir: PathBuf,
        active: Mutex<Vec<Notice>>,
        raised: Mutex<usize>,
    }

    impl LogHost {
        pub fn new(dir: &Path) -> Self {
            Self {
                dir: dir.to_path_buf(),
                active: Mutex::default(),
                raised: Mutex::default(),
            }
        }

        pub fn active(&self) -> Vec<Notice> {
            self.active.lock().unwrap().clone()
        }

        /// How many notices have been raised, including ones since replaced or cleared.
        pub fn raised(&self) -> usize {
            *self.raised.lock().unwrap()
        }
    }

    impl Host for LogHost {
        fn dir(&self) -> Option<PathBuf> {
            Some(self.dir.clone())
        }

        fn raise(&self, notice: Notice) {
            *self.raised.lock().unwrap() += 1;
            insert(&mut self.active.lock().unwrap(), notice);
        }

        fn clear(&self) {
            remove(
                &mut self.active.lock().unwrap(),
                NoticeSource::ReadAloud,
                Some(NoticeCode::VocabularyInvalid),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::scratch;
    use super::*;

    fn load(dir: &Path, reported: &Reported) -> (Arc<Vocabulary>, Vec<FileError>) {
        let mut raised = Vec::new();
        let vocabulary = load_vocabulary(dir, reported, &mut |outcome| {
            if let Outcome::Failed(error) = outcome {
                raised.push(error.clone());
            }
        });
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

    mod non_regular_user_vocabulary_falls_back_without_stalling {
        use super::*;
        use std::sync::mpsc;
        use std::time::Duration;

        fn make_fifo(path: &Path) {
            use std::os::unix::ffi::OsStrExt;
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: c_path is a valid NUL-terminated path that outlives the call.
            let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
        }

        /// Runs the real loader on its own thread; a loader stuck on a FIFO misses the deadline.
        fn load_within_deadline(
            dir: &Path,
            reported: &Arc<Reported>,
        ) -> (Arc<Vocabulary>, Vec<FileError>) {
            let (done, result) = mpsc::channel();
            let dir = dir.to_path_buf();
            let reported = reported.clone();
            std::thread::spawn(move || {
                let mut raised = Vec::new();
                let vocabulary = load_vocabulary(&dir, &reported, &mut |outcome| {
                    if let Outcome::Failed(error) = outcome {
                        raised.push(error.clone());
                    }
                });
                let _ = done.send((vocabulary, raised));
            });
            result
                .recv_timeout(Duration::from_secs(5))
                .expect("the loader stalled on a non-regular file")
        }

        fn assert_refused_once(dir: &Path) {
            let reported = Arc::new(Reported::default());
            let (vocabulary, raised) = load_within_deadline(dir, &reported);
            assert_eq!(*vocabulary, Vocabulary::builtin());
            assert_eq!(raised.len(), 1, "{raised:?}");
            assert!(
                raised[0].to_string().contains("not a regular file"),
                "{}",
                raised[0]
            );
            let (again, raised) = load_within_deadline(dir, &reported);
            assert_eq!(*again, Vocabulary::builtin());
            assert!(raised.is_empty(), "repeated: {raised:?}");
        }

        #[test]
        fn fifo_without_a_writer() {
            let dir = scratch("fifo");
            make_fifo(&dir.join(FILE_NAME));
            assert_refused_once(&dir);
        }

        #[test]
        fn symlink_to_a_fifo() {
            let dir = scratch("fifo-link");
            let target = dir.join("pipe");
            make_fifo(&target);
            std::os::unix::fs::symlink(&target, dir.join(FILE_NAME)).unwrap();
            assert_refused_once(&dir);
        }

        #[test]
        fn symlink_to_a_regular_file_is_followed() {
            let dir = scratch("file-link");
            std::fs::write(
                dir.join("real.json"),
                r#"{ "version": 1, "extensions": { "sh": "shell" } }"#,
            )
            .unwrap();
            std::os::unix::fs::symlink(dir.join("real.json"), dir.join(FILE_NAME)).unwrap();
            let (vocabulary, raised) = load(&dir, &Reported::default());
            assert!(raised.is_empty());
            assert_eq!(spoken(&vocabulary, "lib.sh"), "lib dot shell");
        }

        #[test]
        fn dangling_symlink_is_refused_and_creates_nothing() {
            let dir = scratch("dangling");
            let target = dir.join("missing.json");
            std::os::unix::fs::symlink(&target, dir.join(FILE_NAME)).unwrap();
            let (vocabulary, raised) = load(&dir, &Reported::default());
            assert_eq!(*vocabulary, Vocabulary::builtin());
            assert_eq!(raised.len(), 1, "{raised:?}");
            assert!(!target.exists(), "the link target must not be created");
        }
    }

    mod size_limit {
        use super::*;

        #[test]
        fn file_over_one_mebibyte_falls_back_with_one_notice() {
            let dir = scratch("oversize");
            let mut text = String::from(r#"{ "version": 1, "pad": ""#);
            text.push_str(&"x".repeat(MAX_BYTES as usize));
            text.push_str(r#"" }"#);
            std::fs::write(dir.join(FILE_NAME), text).unwrap();
            let reported = Reported::default();
            let (vocabulary, raised) = load(&dir, &reported);
            assert_eq!(*vocabulary, Vocabulary::builtin());
            assert_eq!(raised.len(), 1, "{raised:?}");
            assert!(
                raised[0].to_string().contains("file larger than 1 MiB"),
                "{}",
                raised[0]
            );
            assert!(load(&dir, &reported).1.is_empty());
        }

        #[test]
        fn file_of_exactly_one_mebibyte_is_read() {
            let dir = scratch("at-limit");
            let head = r#"{ "version": 1, "extensions": { "sh": "shell" }, "#;
            let tail = r#""x": "" }"#;
            let pad = MAX_BYTES as usize - head.len() - tail.len();
            let text = format!("{head}{}{tail}", " ".repeat(pad));
            assert_eq!(text.len() as u64, MAX_BYTES);
            std::fs::write(dir.join(FILE_NAME), text).unwrap();
            let (vocabulary, raised) = load(&dir, &Reported::default());
            // The unknown key "x" is a schema error, which proves the whole file was read.
            assert_eq!(*vocabulary, Vocabulary::builtin());
            assert_eq!(raised.len(), 1, "{raised:?}");
            assert!(matches!(raised[0], FileError::Invalid(_)), "{raised:?}");
        }
    }

    mod created_file {
        use super::*;

        #[test]
        fn an_existing_file_is_never_truncated_by_creation() {
            let dir = scratch("no-truncate");
            let path = dir.join(FILE_NAME);
            std::fs::write(&path, "concurrently written").unwrap();
            create_user_file(&dir, &path);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "concurrently written"
            );
        }
    }

    mod valid_reload_clears_vocabulary_warning {
        use super::*;
        use crate::notices::{NoticeCode, NoticeSource};

        fn codes(host: &testing::LogHost) -> Vec<NoticeCode> {
            host.active().iter().map(|notice| notice.code).collect()
        }

        #[test]
        fn fixed_file_ends_the_warning_and_uses_the_new_entries() {
            let dir = scratch("clear");
            let vocabulary = UserVocabulary::new(testing::LogHost::new(&dir));
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            let broken = vocabulary.load();
            assert_eq!(*broken, Vocabulary::builtin());
            assert_eq!(codes(&vocabulary.host), [NoticeCode::VocabularyInvalid]);
            let detail = vocabulary.host.active()[0].detail.clone().unwrap();
            assert!(detail.contains(FILE_NAME), "{detail}");
            assert!(detail.contains("line 1 column"), "{detail}");

            std::fs::write(
                dir.join(FILE_NAME),
                r#"{ "version": 1, "extensions": { "sh": "shell" } }"#,
            )
            .unwrap();
            let fixed = vocabulary.load();
            assert_eq!(spoken(&fixed, "lib.sh"), "lib dot shell");
            assert!(codes(&vocabulary.host).is_empty());
        }

        #[test]
        fn an_unrelated_read_aloud_notice_survives_the_clear() {
            let dir = scratch("clear-other");
            let vocabulary = UserVocabulary::new(testing::LogHost::new(&dir));
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            let _ = vocabulary.load();
            // One notice per source: a newer Read aloud notice replaces the warning.
            vocabulary.host.raise(Notice::new(
                NoticeCode::ReadAloudBusyPasting,
                NoticeSource::ReadAloud,
            ));
            std::fs::write(dir.join(FILE_NAME), r#"{ "version": 1 }"#).unwrap();
            let _ = vocabulary.load();
            assert_eq!(codes(&vocabulary.host), [NoticeCode::ReadAloudBusyPasting]);
        }

        #[test]
        fn a_file_that_breaks_again_warns_again() {
            let dir = scratch("clear-again");
            let vocabulary = UserVocabulary::new(testing::LogHost::new(&dir));
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            let _ = vocabulary.load();
            std::fs::write(dir.join(FILE_NAME), r#"{ "version": 1 }"#).unwrap();
            let _ = vocabulary.load();
            assert!(codes(&vocabulary.host).is_empty());
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            let _ = vocabulary.load();
            assert_eq!(codes(&vocabulary.host), [NoticeCode::VocabularyInvalid]);
        }

        #[test]
        fn forgetting_makes_a_still_broken_file_warn_again() {
            let dir = scratch("forget");
            let vocabulary = UserVocabulary::new(testing::LogHost::new(&dir));
            std::fs::write(dir.join(FILE_NAME), "{ not json").unwrap();
            let _ = vocabulary.load();
            let _ = vocabulary.load();
            assert_eq!(vocabulary.host.raised(), 1);
            vocabulary.forget();
            let _ = vocabulary.load();
            assert_eq!(vocabulary.host.raised(), 2);
        }
    }
}
