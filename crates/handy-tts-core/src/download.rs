use crate::catalog::{ModelEntry, ModelFile, valid_path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum DownloadError {
    #[error("tts.errors.tokenRequired")]
    TokenRequired,
    #[error("tts.errors.tokenRejected")]
    TokenRejected,
    #[error("tts.errors.download")]
    Transport,
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
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .or_else(|| {
                if self.1 {
                    std::env::var("HF_TOKEN").ok()
                } else {
                    None
                }
            })
            .filter(|value| !value.trim().is_empty())
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
    /// Creates a transport with a bounded request timeout.
    pub fn new(base: &str) -> Result<Self, DownloadError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|_| DownloadError::Transport)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').into(),
        })
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
            return Err(DownloadError::TokenRejected);
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
    if !path.is_file() {
        return Ok(false);
    }
    let mut reader = File::open(path).map_err(|_| DownloadError::Transport)?;
    if reader
        .metadata()
        .map_err(|_| DownloadError::Transport)?
        .len()
        != file.bytes
    {
        return Ok(false);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = reader
            .read(&mut buffer)
            .map_err(|_| DownloadError::Transport)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(format!("{:x}", digest.finalize()) == file.sha256)
}
/// Checks the verification marker and file sizes without rehashing the model.
pub fn is_downloaded(root: &Path, entry: &ModelEntry) -> bool {
    let directory = root.join(entry.id);
    directory.join(".verified").is_file()
        && entry.files.iter().all(|file| {
            fs::metadata(directory.join(&file.path))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.bytes)
        })
}
/// Removes a model directory, including any incomplete files.
pub fn delete(root: &Path, entry: &ModelEntry) -> Result<(), DownloadError> {
    let path = root.join(entry.id);
    if path.exists() {
        fs::remove_dir_all(path).map_err(|_| DownloadError::Transport)?;
    }
    Ok(())
}
#[derive(Default)]
/// Serializes downloads per model while retaining verified files for retries.
pub struct Downloader {
    active: Mutex<std::collections::HashSet<String>>,
    pub credentials: Credentials,
}
struct Claim<'a> {
    downloader: &'a Downloader,
    id: &'a str,
}
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.downloader
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.id);
    }
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
        if !self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(entry.id.into())
        {
            return Err(DownloadError::Busy);
        }
        let _claim = Claim {
            downloader: self,
            id: entry.id,
        };
        let token = self.credentials.token();
        let directory = root.join(entry.id);
        fs::create_dir_all(&directory).map_err(|_| DownloadError::Transport)?;
        match fs::remove_file(directory.join(".verified")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(DownloadError::Transport),
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
            if verify(&final_path, file)? {
                completed += file.bytes;
                continue;
            }
            let partial = final_path.with_file_name(format!(
                "{}.partial",
                final_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(DownloadError::Integrity)?
            ));
            fs::create_dir_all(final_path.parent().ok_or(DownloadError::Integrity)?)
                .map_err(|_| DownloadError::Transport)?;
            if verify(&partial, file)? {
                fs::rename(&partial, &final_path).map_err(|_| DownloadError::Transport)?;
                completed += file.bytes;
                continue;
            }
            if file.gated && token.is_none() {
                return Err(DownloadError::TokenRequired);
            }
            if fs::metadata(&partial).is_ok_and(|metadata| metadata.len() >= file.bytes) {
                fs::remove_file(&partial).map_err(|_| DownloadError::Transport)?;
            }
            let offset = fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
            let mut response = transport.get(file, offset, token.as_deref())?;
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
                .map_err(|_| DownloadError::Transport)?;
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
                    .map_err(|_| DownloadError::Transport)?;
                progress(Progress {
                    model_id: entry.id.into(),
                    downloaded_bytes: completed + count,
                    total_bytes: entry.bytes,
                });
            }
            output.sync_all().map_err(|_| DownloadError::Transport)?;
            if !verify(&partial, file)? {
                let _ = fs::remove_file(&partial);
                return Err(DownloadError::Integrity);
            }
            fs::rename(&partial, &final_path).map_err(|_| DownloadError::Transport)?;
            completed += file.bytes;
        }
        fs::write(directory.join(".verified"), b"verified\n")
            .map_err(|_| DownloadError::Transport)?;
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
        for stream in [&output.stdout, &output.stderr] {
            assert!(
                !String::from_utf8_lossy(stream).contains("hf_fixture_secret_never_log"),
                "HTTP errors must not expose credentials"
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let capture = requests.clone();
        let server = std::thread::spawn(move || {
            for body in [b"public".as_slice(), b"voice".as_slice()] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut one = [0];
                    socket.read_exact(&mut one).unwrap();
                    bytes.push(one[0]);
                }
                capture
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(bytes).unwrap());
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(body).unwrap();
            }
        });
        let root = root("gated-http");
        let downloader = Downloader {
            credentials: Credentials::memory_only(),
            ..Downloader::default()
        };
        let transport = HttpTransport::new(&base).unwrap();
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
        server.join().unwrap();
        let captured = requests.lock().unwrap();
        assert!(!captured[0].to_lowercase().contains("authorization:"));
        assert!(captured[1].contains(secret));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "http://{}/hf_fixture_secret_never_log",
            listener.local_addr().unwrap()
        );
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut byte = [0];
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            socket.write_all(b"invalid HTTP response\r\n\r\n").unwrap();
        });
        let result = HttpTransport::new(&base).unwrap().get(
            &fixture().files[1],
            0,
            Some("hf_fixture_secret_never_log"),
        );
        server.join().unwrap();
        let error = result.err().expect("malformed HTTP response is rejected");
        assert_eq!(error, DownloadError::Transport);
        assert_eq!(
            format!("{error} {error:?}"),
            "tts.errors.download Transport"
        );
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
        let downloader = Downloader::default();
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
        let result = Downloader::default().download(
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
        Downloader::default()
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
        let result = Downloader::default().download(
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
        Downloader::default()
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
        let downloader = Downloader::default();
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
}
