use crate::catalog::{ModelEntry, ModelFile, valid_path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum DownloadError {
    #[error("tts.errors.tokenRequired")]
    TokenRequired,
    #[error("tts.errors.tokenRejected")]
    TokenRejected,
    #[error("tts.errors.download")]
    Transport,
    #[error("tts.errors.accessDenied")]
    AccessDenied,
    #[error("tts.errors.localIo")]
    LocalIo,
    #[error("tts.errors.integrity")]
    Integrity,
    #[error("tts.errors.cancelled")]
    Cancelled,
    #[error("tts.errors.busy")]
    Busy,
}
/// Keeps download credentials in memory without exposing them through Debug.
#[derive(Clone)]
pub struct Credentials(Arc<Mutex<Option<String>>>, bool);
impl Default for Credentials {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)), true)
    }
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Credentials([redacted])")
    }
}
impl Credentials {
    /// Creates credentials that never consult HF_TOKEN.
    pub fn memory_only() -> Self {
        Self(Arc::new(Mutex::new(None)), false)
    }
    /// Replaces the session token, clearing it when the input is blank.
    pub fn set(&self, value: String) {
        let value = value.trim().to_owned();
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = if value.trim().is_empty() {
            None
        } else {
            Some(value)
        };
    }
    /// Reports whether a session token or an enabled HF_TOKEN is available.
    pub fn configured(&self) -> bool {
        self.token().is_some()
    }
    fn token(&self) -> Option<String> {
        self.token_with(|| std::env::var("HF_TOKEN").ok())
    }
    fn token_with(&self, environment: impl FnOnce() -> Option<String>) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .or_else(|| if self.1 { environment() } else { None })
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Reports accepted bytes across all files in one model download.
pub struct Progress {
    pub model_id: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}
/// Carries the HTTP status, range start and streaming response body.
pub struct Response {
    pub status: u16,
    pub start: Option<u64>,
    pub body: Box<dyn Read + Send>,
}
/// Fetches pinned files without exposing credentials in errors.
pub trait Transport: Send + Sync {
    fn get(
        &self,
        file: &ModelFile,
        offset: u64,
        token: Option<&str>,
    ) -> Result<Response, DownloadError>;
}
/// Fetches model files from a Hugging Face compatible resolve endpoint.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    base: String,
}
impl HttpTransport {
    /// Creates an HTTPS transport with bounded connection and read waits.
    pub fn new(base: &str) -> Result<Self, DownloadError> {
        let url = reqwest::Url::parse(base).map_err(|_| DownloadError::Transport)?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err(DownloadError::Transport);
        }
        Self::build(base, Duration::from_secs(60))
    }
    fn build(base: &str, read_timeout: Duration) -> Result<Self, DownloadError> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Blocking reqwest applies this timeout separately to each body read.
            .timeout(read_timeout)
            .build()
            .map_err(|_| DownloadError::Transport)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').into(),
        })
    }
    #[cfg(test)]
    fn loopback(base: &str) -> Result<Self, DownloadError> {
        let url = reqwest::Url::parse(base).map_err(|_| DownloadError::Transport)?;
        let loopback = url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if url.scheme() != "http" || !loopback {
            return Err(DownloadError::Transport);
        }
        Self::build(base, Duration::from_secs(60))
    }
}
impl Transport for HttpTransport {
    fn get(
        &self,
        file: &ModelFile,
        offset: u64,
        token: Option<&str>,
    ) -> Result<Response, DownloadError> {
        let url = format!(
            "{}/{}/resolve/{}/{}",
            self.base, file.repo, file.revision, file.path
        );
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if offset != 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        if file.gated {
            request = request.bearer_auth(token.ok_or(DownloadError::TokenRequired)?);
        }
        let response = request.send().map_err(|_| DownloadError::Transport)?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(if file.gated {
                DownloadError::TokenRejected
            } else {
                DownloadError::AccessDenied
            });
        }
        let start = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes "))
            .and_then(|value| value.split('-').next())
            .and_then(|value| value.parse().ok());
        Ok(Response {
            status,
            start,
            body: Box::new(response),
        })
    }
}
/// Verifies both the exact byte count and SHA-256 of a downloaded file.
pub fn verify(path: &Path, file: &ModelFile) -> Result<bool, DownloadError> {
    let mut reader = match File::open(path) {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(DownloadError::LocalIo),
    };
    let metadata = reader.metadata().map_err(|_| DownloadError::LocalIo)?;
    if !metadata.is_file() || metadata.len() != file.bytes {
        return Ok(false);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = reader
            .read(&mut buffer)
            .map_err(|_| DownloadError::LocalIo)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(format!("{:x}", digest.finalize()) == file.sha256)
}
fn fingerprint(entry: &ModelEntry) -> String {
    const VERSION: &str = "handy-tts-manifest-v1";
    let mut files: Vec<_> = entry
        .files
        .iter()
        .map(|file| {
            (
                file.revision.as_str(),
                file.path.as_str(),
                file.bytes,
                file.sha256.as_str(),
            )
        })
        .collect();
    files.sort_unstable();
    let mut digest = Sha256::new();
    digest.update(VERSION.as_bytes());
    for (revision, path, bytes, sha256) in files {
        for value in [revision, path, sha256] {
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
        digest.update(bytes.to_le_bytes());
    }
    format!("{VERSION}\n{:x}\n", digest.finalize())
}
/// Checks the manifest fingerprint and file sizes without rehashing the model.
pub fn is_downloaded(root: &Path, entry: &ModelEntry) -> bool {
    let directory = root.join(entry.id);
    fs::read_to_string(directory.join(".verified")).is_ok_and(|marker| marker == fingerprint(entry))
        && entry.files.iter().all(|file| {
            fs::metadata(directory.join(&file.path))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.bytes)
        })
}
fn remove_file_if_present(path: &Path) -> Result<(), DownloadError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(DownloadError::LocalIo),
    }
}
static CLAIMS: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
struct Claim(PathBuf);
impl Claim {
    fn acquire(directory: PathBuf) -> Result<Self, DownloadError> {
        let active = CLAIMS.get_or_init(Mutex::default);
        if !active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(directory.clone())
        {
            return Err(DownloadError::Busy);
        }
        Ok(Self(directory))
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(active) = CLAIMS.get() {
            active
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&self.0);
        }
    }
}
/// Removes an unclaimed model directory, including any incomplete files.
pub fn delete(root: &Path, entry: &ModelEntry) -> Result<(), DownloadError> {
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(DownloadError::LocalIo),
    };
    let path = root.join(entry.id);
    let _claim = Claim::acquire(path.clone())?;
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(DownloadError::LocalIo),
    }
}
/// Serializes downloads per model directory while retaining verified files for retries.
#[derive(Default)]
pub struct Downloader {
    pub credentials: Credentials,
}
impl Downloader {
    /// Downloads and verifies every file before marking a model available.
    pub fn download(
        &self,
        root: &Path,
        entry: &ModelEntry,
        transport: &dyn Transport,
        cancel: &AtomicBool,
        progress: impl Fn(Progress),
    ) -> Result<PathBuf, DownloadError> {
        fs::create_dir_all(root).map_err(|_| DownloadError::LocalIo)?;
        let directory = root
            .canonicalize()
            .map_err(|_| DownloadError::LocalIo)?
            .join(entry.id);
        let _claim = Claim::acquire(directory.clone())?;
        let token = self.credentials.token();
        fs::create_dir_all(&directory).map_err(|_| DownloadError::LocalIo)?;
        let marker = directory.join(".verified");
        if !is_downloaded(root, entry) {
            remove_file_if_present(&marker)?;
        }
        let mut completed = 0u64;
        for file in &entry.files {
            if cancel.load(Ordering::Acquire) {
                return Err(DownloadError::Cancelled);
            }
            if !valid_path(&file.path) {
                return Err(DownloadError::Integrity);
            }
            let final_path = directory.join(&file.path);
            match verify(&final_path, file) {
                Ok(true) => {
                    completed += file.bytes;
                    progress(Progress {
                        model_id: entry.id.into(),
                        downloaded_bytes: completed,
                        total_bytes: entry.bytes,
                    });
                    continue;
                }
                Ok(false) => remove_file_if_present(&marker)?,
                Err(error) => {
                    remove_file_if_present(&marker)?;
                    return Err(error);
                }
            }
            let partial = final_path.with_file_name(format!(
                "{}.partial",
                final_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(DownloadError::Integrity)?
            ));
            fs::create_dir_all(final_path.parent().ok_or(DownloadError::Integrity)?)
                .map_err(|_| DownloadError::LocalIo)?;
            if verify(&partial, file)? {
                fs::rename(&partial, &final_path).map_err(|_| DownloadError::LocalIo)?;
                completed += file.bytes;
                continue;
            }
            if file.gated && token.is_none() {
                return Err(DownloadError::TokenRequired);
            }
            let mut offset = match fs::metadata(&partial) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(_) => return Err(DownloadError::LocalIo),
            };
            if offset >= file.bytes {
                remove_file_if_present(&partial)?;
                offset = 0;
            }
            let mut response = transport.get(file, offset, token.as_deref())?;
            if response.status == 416 || (response.status == 206 && response.start != Some(offset))
            {
                drop(response);
                remove_file_if_present(&partial)?;
                offset = 0;
                if cancel.load(Ordering::Acquire) {
                    return Err(DownloadError::Cancelled);
                }
                response = transport.get(file, offset, token.as_deref())?;
            }
            let resume = response.status == 206 && response.start == Some(offset);
            if response.status != 200 && !resume {
                return Err(DownloadError::Transport);
            }
            let mut output = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(!resume)
                .append(resume)
                .open(&partial)
                .map_err(|_| DownloadError::LocalIo)?;
            let mut count = if resume { offset } else { 0 };
            let mut buffer = [0u8; 65536];
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(DownloadError::Cancelled);
                }
                let size = response
                    .body
                    .read(&mut buffer)
                    .map_err(|_| DownloadError::Transport)?;
                if size == 0 {
                    break;
                }
                count = count
                    .checked_add(size as u64)
                    .ok_or(DownloadError::Integrity)?;
                if count > file.bytes {
                    return Err(DownloadError::Integrity);
                }
                output
                    .write_all(&buffer[..size])
                    .map_err(|_| DownloadError::LocalIo)?;
                progress(Progress {
                    model_id: entry.id.into(),
                    downloaded_bytes: completed + count,
                    total_bytes: entry.bytes,
                });
            }
            output.sync_all().map_err(|_| DownloadError::LocalIo)?;
            if !verify(&partial, file)? {
                remove_file_if_present(&partial)?;
                return Err(DownloadError::Integrity);
            }
            fs::rename(&partial, &final_path).map_err(|_| DownloadError::LocalIo)?;
            completed += file.bytes;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(DownloadError::Cancelled);
        }
        fs::write(&marker, fingerprint(entry)).map_err(|_| DownloadError::LocalIo)?;
        progress(Progress {
            model_id: entry.id.into(),
            downloaded_bytes: entry.bytes,
            total_bytes: entry.bytes,
        });
        Ok(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    fn root(name: &str) -> PathBuf {
        let scratch = std::env::var_os("HANDY_TTS_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.superpowers/scratch/read-aloud")
            });
        let root = scratch
            .join("download-tests")
            .join(format!("{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }
    fn fixture() -> ModelEntry {
        let mut entry = crate::catalog::entry("pocket-en").unwrap().clone();
        entry.files = [
            ("public.bin", false, b"public".as_slice()),
            ("voices/alba.bin", true, b"voice".as_slice()),
        ]
        .into_iter()
        .map(|(path, gated, body)| ModelFile {
            model_id: entry.id.into(),
            repo: "fixture/model".into(),
            revision: "a".repeat(40),
            path: path.into(),
            bytes: body.len() as u64,
            sha256: format!("{:x}", Sha256::digest(body)),
            gated,
        })
        .collect();
        entry.bytes = entry.files.iter().map(|file| file.bytes).sum();
        entry
    }
    #[test]
    fn gated_http_download_requires_token_resumes_public_and_never_exposes_secret() {
        const CHILD: &str = "HANDY_TTS_REDACTION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            transport_errors_are_redacted();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "download::tests::gated_http_download_requires_token_resumes_public_and_never_exposes_secret",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "transport error fixture failed");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
            "redaction child must run one test"
        );
        for stream in [&output.stdout, &output.stderr] {
            assert!(
                !String::from_utf8_lossy(stream).contains("hf_fixture_secret_never_log"),
                "HTTP errors must not expose credentials"
            );
        }
        let server = TestServer::start(vec![reply(200, "", b"public"), reply(200, "", b"voice")]);
        let requests = server.requests.clone();
        let root = root("gated-http");
        let downloader = Downloader {
            credentials: Credentials::memory_only(),
        };
        let transport = HttpTransport::loopback(&server.base).unwrap();
        let entry = fixture();
        let result =
            downloader.download(&root, &entry, &transport, &AtomicBool::new(false), |_| {});
        assert_eq!(result.unwrap_err(), DownloadError::TokenRequired);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(verify(&root.join(entry.id).join("public.bin"), &entry.files[0]).unwrap());
        assert!(!is_downloaded(&root, &entry));
        let secret = "hf_fixture_secret_never_log";
        downloader.credentials.set(secret.into());
        let progress = Mutex::new(Vec::new());
        downloader
            .download(
                &root,
                &entry,
                &transport,
                &AtomicBool::new(false),
                |value| progress.lock().unwrap().push(value),
            )
            .unwrap();
        server.finish();
        let captured = requests.lock().unwrap();
        assert!(!captured[0].to_lowercase().contains("authorization:"));
        assert_authorization_only(&captured[1], Some(secret));
        let logs = format!(
            "{:?} {:?} {:?}",
            downloader.credentials,
            DownloadError::TokenRequired,
            progress.lock().unwrap()
        );
        assert!(!logs.contains(secret));
        assert!(is_downloaded(&root, &entry));
        fs::write(root.join(entry.id).join("voices/alba.bin"), b"wrong").unwrap();
        assert!(
            !verify(
                &root.join(entry.id).join("voices/alba.bin"),
                &entry.files[1]
            )
            .unwrap()
        );
        downloader.credentials.set(String::new());
        assert_eq!(
            downloader
                .download(&root, &entry, &transport, &AtomicBool::new(false), |_| {})
                .unwrap_err(),
            DownloadError::TokenRequired
        );
        assert!(
            !is_downloaded(&root, &entry),
            "failed revalidation clears the marker"
        );
        fs::remove_dir_all(root).unwrap();
    }
    fn transport_errors_are_redacted() {
        static LOGGER: CapturingLogger = CapturingLogger(Mutex::new(Vec::new()));
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
        log::info!("capture-probe");
        let server = TestServer::start(vec!["invalid HTTP response\r\n\r\n".into()]);
        let base = format!("{}/hf_fixture_secret_never_log", server.base);
        let result = HttpTransport::loopback(&base).unwrap().get(
            &fixture().files[1],
            0,
            Some("hf_fixture_secret_never_log"),
        );
        server.finish();
        let error = result.err().expect("malformed HTTP response is rejected");
        assert_eq!(error, DownloadError::Transport);
        assert_eq!(
            format!("{error} {error:?}"),
            "tts.errors.download Transport"
        );
        let logs = LOGGER.0.lock().unwrap();
        assert!(logs.iter().any(|line| line.contains("capture-probe")));
        assert!(
            !logs
                .iter()
                .any(|line| line.contains("hf_fixture_secret_never_log")),
            "logger must not expose credentials"
        );
    }
    struct CapturingLogger(Mutex<Vec<String>>);
    impl log::Log for CapturingLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    struct MemoryTransport {
        body: Vec<u8>,
        offset: u64,
        status: u16,
    }
    struct RefuseTransport;
    impl Transport for RefuseTransport {
        fn get(&self, _: &ModelFile, _: u64, _: Option<&str>) -> Result<Response, DownloadError> {
            Err(DownloadError::Transport)
        }
    }
    impl Transport for MemoryTransport {
        fn get(
            &self,
            _: &ModelFile,
            offset: u64,
            _: Option<&str>,
        ) -> Result<Response, DownloadError> {
            assert_eq!(offset, self.offset);
            Ok(Response {
                status: self.status,
                start: Some(offset),
                body: Box::new(std::io::Cursor::new(self.body.clone())),
            })
        }
    }
    #[test]
    fn range_resume_size_hash_cancel_and_marker_guards() {
        oversized_responses_stop_before_write_or_progress();
        full_responses_replace_partial_files();
        cancellation_during_read_preserves_partial_without_marker();
        same_model_download_is_busy_until_claim_is_released();
        let root = root("range");
        let mut entry = fixture();
        entry.files.truncate(1);
        entry.bytes = 6;
        let directory = root.join(entry.id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("public.bin.partial"), b"pub").unwrap();
        let downloader = downloader_without_environment();
        downloader
            .download(
                &root,
                &entry,
                &MemoryTransport {
                    body: b"lic".to_vec(),
                    offset: 3,
                    status: 206,
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        assert!(is_downloaded(&root, &entry));
        fs::remove_file(directory.join(".verified")).unwrap();
        assert!(!is_downloaded(&root, &entry));
        fs::remove_file(directory.join("public.bin")).unwrap();
        assert_eq!(
            downloader
                .download(
                    &root,
                    &entry,
                    &MemoryTransport {
                        body: b"wrong!".to_vec(),
                        offset: 0,
                        status: 200
                    },
                    &AtomicBool::new(false),
                    |_| {}
                )
                .unwrap_err(),
            DownloadError::Integrity
        );
        assert!(!directory.join("public.bin.partial").exists());
        assert_eq!(
            downloader
                .download(
                    &root,
                    &entry,
                    &MemoryTransport {
                        body: vec![],
                        offset: 0,
                        status: 200
                    },
                    &AtomicBool::new(true),
                    |_| {}
                )
                .unwrap_err(),
            DownloadError::Cancelled
        );
        fs::remove_dir_all(root).unwrap();
    }
    fn public_fixture() -> ModelEntry {
        let mut entry = fixture();
        entry.files.truncate(1);
        entry.bytes = entry.files[0].bytes;
        entry
    }
    fn oversized_responses_stop_before_write_or_progress() {
        let root = root("oversized");
        let entry = public_fixture();
        let progress = Mutex::new(Vec::new());
        let result = downloader_without_environment().download(
            &root,
            &entry,
            &MemoryTransport {
                body: b"public-extra".to_vec(),
                offset: 0,
                status: 200,
            },
            &AtomicBool::new(false),
            |value| progress.lock().unwrap().push(value),
        );
        assert_eq!(result.unwrap_err(), DownloadError::Integrity);
        assert!(
            progress.lock().unwrap().is_empty(),
            "oversized data is never accepted"
        );
        let partial = root.join(entry.id).join("public.bin.partial");
        assert!(fs::metadata(partial).map_or(true, |m| m.len() <= entry.bytes));
        assert!(!is_downloaded(&root, &entry));
        fs::remove_dir_all(root).unwrap();
    }
    fn full_responses_replace_partial_files() {
        let root = root("range-ignored");
        let entry = public_fixture();
        let directory = root.join(entry.id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("public.bin.partial"), b"pub").unwrap();
        downloader_without_environment()
            .download(
                &root,
                &entry,
                &MemoryTransport {
                    body: b"public".to_vec(),
                    offset: 3,
                    status: 200,
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        assert_eq!(fs::read(directory.join("public.bin")).unwrap(), b"public");
        assert!(is_downloaded(&root, &entry));
        fs::remove_dir_all(root).unwrap();
    }
    fn cancellation_during_read_preserves_partial_without_marker() {
        let root = root("cancel-during-read");
        let entry = public_fixture();
        let cancel = AtomicBool::new(false);
        let result = downloader_without_environment().download(
            &root,
            &entry,
            &MemoryTransport {
                body: b"public".to_vec(),
                offset: 0,
                status: 200,
            },
            &cancel,
            |_| cancel.store(true, Ordering::Release),
        );
        assert_eq!(result.unwrap_err(), DownloadError::Cancelled);
        assert!(!is_downloaded(&root, &entry));
        assert!(!root.join(entry.id).join("public.bin").exists());
        assert_eq!(
            fs::read(root.join(entry.id).join("public.bin.partial")).unwrap(),
            b"public"
        );
        downloader_without_environment()
            .download(
                &root,
                &entry,
                &RefuseTransport,
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        assert!(
            is_downloaded(&root, &entry),
            "a complete partial needs no new request"
        );
        fs::remove_dir_all(root).unwrap();
    }
    fn same_model_download_is_busy_until_claim_is_released() {
        let root = root("busy");
        let entry = public_fixture();
        let downloader = downloader_without_environment();
        let transport = MemoryTransport {
            body: b"public".to_vec(),
            offset: 0,
            status: 200,
        };
        let cancel = AtomicBool::new(false);
        downloader
            .download(&root, &entry, &transport, &cancel, |_| {
                assert_eq!(
                    downloader
                        .download(&root, &entry, &RefuseTransport, &cancel, |_| {})
                        .unwrap_err(),
                    DownloadError::Busy
                );
            })
            .unwrap();
        assert!(
            downloader
                .download(&root, &entry, &transport, &cancel, |_| {})
                .is_ok()
        );
        delete(&root, &entry).unwrap();
        assert!(!is_downloaded(&root, &entry));
        delete(&root, &entry).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    struct TestServer {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
        done: std::sync::mpsc::Receiver<()>,
        thread: std::thread::JoinHandle<()>,
    }
    impl TestServer {
        fn start(replies: Vec<String>) -> Self {
            Self::start_chunks(
                replies
                    .into_iter()
                    .map(|reply| vec![(Duration::ZERO, reply)])
                    .collect(),
            )
        }
        fn start_chunks(replies: Vec<Vec<(Duration, String)>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            let (sender, done) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                for reply in replies {
                    let mut socket = loop {
                        match listener.accept() {
                            Ok((socket, _)) => break socket,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "HTTP fixture request missing"
                                );
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            Err(error) => panic!("fixture accept: {error}"),
                        }
                    };
                    socket
                        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(std::time::Duration::from_secs(1)))
                        .unwrap();
                    let mut request = Vec::new();
                    for _ in 0..16384 {
                        let mut byte = [0];
                        socket.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    assert!(request.ends_with(b"\r\n\r\n"));
                    captured
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(request).unwrap());
                    for (delay, part) in reply {
                        std::thread::sleep(delay);
                        if socket.write_all(part.as_bytes()).is_err() {
                            break;
                        }
                    }
                }
                sender.send(()).unwrap();
            });
            Self {
                base,
                requests,
                done,
                thread,
            }
        }
        fn finish(self) {
            let completion = self.done.recv_timeout(std::time::Duration::from_secs(5));
            assert!(
                completion.is_ok(),
                "HTTP fixture did not finish before deadline"
            );
            self.thread.join().unwrap();
        }
    }
    fn reply(status: u16, headers: &str, body: &[u8]) -> String {
        format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )
    }
    fn assert_authorization_only(request: &str, token: Option<&str>) {
        let mut lines = request.lines();
        let target = lines.next().unwrap();
        let auth: Vec<_> = lines
            .filter(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .collect();
        match token {
            Some(token) => {
                assert!(!target.contains(token), "credentials must never enter URLs");
                assert_eq!(auth.len(), 1);
                assert_eq!(
                    auth[0].split_once(':').unwrap().1.trim(),
                    format!("Bearer {token}")
                );
                assert_eq!(
                    request.matches(token).count(),
                    1,
                    "only Authorization contains the token"
                );
            }
            None => assert!(auth.is_empty()),
        }
    }
    fn downloader_without_environment() -> Downloader {
        Downloader {
            credentials: Credentials::memory_only(),
        }
    }
    #[test]
    fn http_resume_and_gated_rejections_use_headers_only() {
        let root = root("http-range");
        let entry = public_fixture();
        let directory = root.join(entry.id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("public.bin.partial"), b"pub").unwrap();
        let server = TestServer::start(vec![reply(206, "Content-Range: bytes 3-5/6\r\n", b"lic")]);
        let requests = server.requests.clone();
        downloader_without_environment()
            .download(
                &root,
                &entry,
                &HttpTransport::loopback(&server.base).unwrap(),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        server.finish();
        assert_eq!(fs::read(directory.join("public.bin")).unwrap(), b"public");
        assert!(
            requests.lock().unwrap()[0]
                .to_ascii_lowercase()
                .contains("range: bytes=3-")
        );
        assert_authorization_only(&requests.lock().unwrap()[0], None);
        for status in [401, 403] {
            for gated in [true, false] {
                let server = TestServer::start(vec![reply(status, "", b"")]);
                let requests = server.requests.clone();
                let mut file = entry.files[0].clone();
                file.gated = gated;
                let error = HttpTransport::loopback(&server.base)
                    .unwrap()
                    .get(&file, 0, Some("hf_fixture_header_only"))
                    .err()
                    .unwrap();
                server.finish();
                assert_eq!(
                    error,
                    if gated {
                        DownloadError::TokenRejected
                    } else {
                        DownloadError::AccessDenied
                    }
                );
                assert_authorization_only(
                    &requests.lock().unwrap()[0],
                    gated.then_some("hf_fixture_header_only"),
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn marker_binds_every_manifest_field_and_version_in_sorted_order() {
        let root = root("fingerprint");
        let entry = public_fixture();
        let downloader = downloader_without_environment();
        let transport = MemoryTransport {
            body: b"public".to_vec(),
            offset: 0,
            status: 200,
        };
        downloader
            .download(&root, &entry, &transport, &AtomicBool::new(false), |_| {})
            .unwrap();
        assert!(is_downloaded(&root, &entry));
        for field in ["revision", "path", "bytes", "sha256"] {
            let mut changed = entry.clone();
            match field {
                "revision" => changed.files[0].revision = "b".repeat(40),
                "path" => {
                    changed.files[0].path = "same-size.bin".into();
                    fs::write(root.join(entry.id).join("same-size.bin"), b"public").unwrap();
                }
                "bytes" => changed.files[0].bytes += 1,
                "sha256" => changed.files[0].sha256 = format!("{:x}", Sha256::digest(b"repin!")),
                _ => unreachable!(),
            }
            assert!(!is_downloaded(&root, &changed), "marker must bind {field}");
        }
        let marker_path = root.join(entry.id).join(".verified");
        let marker = fs::read(&marker_path).unwrap();
        fs::write(&marker_path, b"verified\n").unwrap();
        assert!(
            !is_downloaded(&root, &entry),
            "legacy marker has no fingerprint"
        );
        fs::write(&marker_path, &marker).unwrap();
        let mut repinned = entry.clone();
        repinned.files[0].revision = "b".repeat(40);
        repinned.files[0].sha256 = format!("{:x}", Sha256::digest(b"repin!"));
        downloader
            .download(
                &root,
                &repinned,
                &MemoryTransport {
                    body: b"repin!".to_vec(),
                    offset: 0,
                    status: 200,
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        assert_eq!(
            fs::read(root.join(entry.id).join("public.bin")).unwrap(),
            b"repin!"
        );
        assert!(is_downloaded(&root, &repinned));
        assert_ne!(fs::read(marker_path).unwrap(), marker);
        let mut pair = repinned.clone();
        let mut second = pair.files[0].clone();
        second.path = "second.bin".into();
        pair.files.push(second);
        pair.bytes *= 2;
        downloader
            .download(
                &root,
                &pair,
                &MemoryTransport {
                    body: b"repin!".to_vec(),
                    offset: 0,
                    status: 200,
                },
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        pair.files.reverse();
        assert!(
            is_downloaded(&root, &pair),
            "manifest order must not change its identity"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn credentials_trim_memory_and_environment_seam_without_reading_environment() {
        let credentials = Credentials::memory_only();
        credentials.set(" \thf_fixture_trim\n".into());
        assert_eq!(credentials.token().as_deref(), Some("hf_fixture_trim"));
        credentials.set(" \n\t".into());
        assert!(!credentials.configured());
        let credentials = Credentials::default();
        assert_eq!(
            credentials
                .token_with(|| Some(" \thf_fixture_trim\n".into()))
                .as_deref(),
            Some("hf_fixture_trim")
        );
        assert!(credentials.token_with(|| Some(" \n\t".into())).is_none());
        assert!(
            Credentials::memory_only()
                .token_with(|| panic!("environment must not be consulted"))
                .is_none()
        );
    }
    #[test]
    fn production_transport_requires_https_and_test_http_is_loopback_only() {
        for base in [
            "http://huggingface.co",
            "http://127.0.0.1:1234",
            "ftp://huggingface.co",
            "not a url",
        ] {
            assert!(
                HttpTransport::new(base).is_err(),
                "production rejects {base}"
            );
        }
        assert!(HttpTransport::new("https://huggingface.co").is_ok());
        assert!(HttpTransport::loopback("http://127.0.0.1:1234").is_ok());
        assert!(HttpTransport::loopback("http://example.com").is_err());
    }
    #[test]
    fn delete_is_busy_and_cancelled_intact_recheck_preserves_marker() {
        let root = root("delete-recheck");
        let mut entry = public_fixture();
        let mut second = entry.files[0].clone();
        second.path = "second.bin".into();
        entry.files.push(second);
        entry.bytes *= 2;
        let downloader = downloader_without_environment();
        downloader
            .download(
                &root,
                &entry,
                &MemoryTransport {
                    body: b"public".to_vec(),
                    offset: 0,
                    status: 200,
                },
                &AtomicBool::new(false),
                |_| {
                    assert_eq!(delete(&root, &entry), Err(DownloadError::Busy));
                },
            )
            .unwrap();
        let marker = fs::read(root.join(entry.id).join(".verified")).unwrap();
        for pre_cancel in [true, false] {
            let cancel = AtomicBool::new(pre_cancel);
            assert_eq!(
                downloader
                    .download(&root, &entry, &RefuseTransport, &cancel, |_| cancel
                        .store(true, Ordering::Release))
                    .unwrap_err(),
                DownloadError::Cancelled
            );
            assert_eq!(
                fs::read(root.join(entry.id).join(".verified")).unwrap(),
                marker
            );
            assert!(is_downloaded(&root, &entry));
        }
        delete(&root, &entry).unwrap();
        assert!(!is_downloaded(&root, &entry));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bad_ranges_discard_partial_and_restart_once() {
        for (name, first) in [
            (
                "mismatch",
                reply(206, "Content-Range: bytes 1-3/6\r\n", b"ubl"),
            ),
            ("missing-range", reply(206, "", b"lic")),
            ("416", reply(416, "", b"")),
        ] {
            let root = root(name);
            let entry = public_fixture();
            let directory = root.join(entry.id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("public.bin.partial"), b"bad").unwrap();
            let server = TestServer::start(vec![first, reply(200, "", b"public")]);
            let requests = server.requests.clone();
            let partial = directory.join("public.bin.partial");
            struct CheckDiscard<'a>(&'a Path, HttpTransport, AtomicBool);
            impl Transport for CheckDiscard<'_> {
                fn get(
                    &self,
                    file: &ModelFile,
                    offset: u64,
                    token: Option<&str>,
                ) -> Result<Response, DownloadError> {
                    if self.2.swap(true, Ordering::Relaxed) {
                        assert_eq!(offset, 0);
                        assert!(
                            !self.0.exists(),
                            "mismatched partial must be discarded before restart"
                        );
                    }
                    self.1.get(file, offset, token)
                }
            }
            let transport = CheckDiscard(
                &partial,
                HttpTransport::loopback(&server.base).unwrap(),
                AtomicBool::new(false),
            );
            downloader_without_environment()
                .download(&root, &entry, &transport, &AtomicBool::new(false), |_| {})
                .unwrap();
            server.finish();
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].to_ascii_lowercase().contains("range: bytes=3-"));
            assert!(!requests[1].to_ascii_lowercase().contains("range:"));
            assert_eq!(fs::read(directory.join("public.bin")).unwrap(), b"public");
            fs::remove_dir_all(root).unwrap();
        }
        let root = root("range-retry-limit");
        let entry = public_fixture();
        struct RejectedRanges(std::sync::atomic::AtomicUsize);
        impl Transport for RejectedRanges {
            fn get(
                &self,
                _: &ModelFile,
                _: u64,
                _: Option<&str>,
            ) -> Result<Response, DownloadError> {
                assert!(
                    self.0.fetch_add(1, Ordering::Relaxed) < 2,
                    "only one range restart is allowed"
                );
                Ok(Response {
                    status: 416,
                    start: None,
                    body: Box::new(std::io::empty()),
                })
            }
        }
        let transport = RejectedRanges(std::sync::atomic::AtomicUsize::new(0));
        assert_eq!(
            downloader_without_environment()
                .download(&root, &entry, &transport, &AtomicBool::new(false), |_| {})
                .unwrap_err(),
            DownloadError::Transport
        );
        assert_eq!(transport.0.load(Ordering::Relaxed), 2);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn local_io_failures_are_distinct_from_network_errors() {
        let root = root("local-io");
        let entry = public_fixture();
        let directory = root.join(entry.id);
        fs::write(&directory, b"not a directory").unwrap();
        let result = downloader_without_environment().download(
            &root,
            &entry,
            &RefuseTransport,
            &AtomicBool::new(false),
            |_| {},
        );
        assert_eq!(result.unwrap_err(), DownloadError::LocalIo);
        assert_eq!(
            verify(&directory.join("child"), &entry.files[0]),
            Err(DownloadError::LocalIo)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn blocking_timeout_is_per_read_and_stalls_are_bounded() {
        let header = "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\n";
        let mut chunks = vec![(Duration::ZERO, header.into())];
        chunks.extend(
            "public"
                .chars()
                .map(|c| (Duration::from_millis(100), c.to_string())),
        );
        let server = TestServer::start_chunks(vec![chunks]);
        let root = root("slow-stream");
        let entry = public_fixture();
        let transport = HttpTransport::build(&server.base, Duration::from_millis(350)).unwrap();
        downloader_without_environment()
            .download(&root, &entry, &transport, &AtomicBool::new(false), |_| {})
            .unwrap();
        server.finish();
        assert!(is_downloaded(&root, &entry));
        fs::remove_dir_all(root).unwrap();

        let server = TestServer::start_chunks(vec![vec![
            (Duration::ZERO, header.into()),
            (Duration::from_millis(700), "public".into()),
        ]]);
        let transport = HttpTransport::build(&server.base, Duration::from_millis(350)).unwrap();
        let mut response = transport.get(&entry.files[0], 0, None).unwrap();
        let started = std::time::Instant::now();
        assert!(response.body.read(&mut [0; 6]).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        server.finish();
    }
}
