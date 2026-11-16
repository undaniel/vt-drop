//! Minimal blocking VirusTotal API v3 client (rustls via `ureq`).

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::model::Stats;

const API_BASE: &str = "https://www.virustotal.com/api/v3";
const LARGE_FILE_THRESHOLD: u64 = 32 * 1024 * 1024;
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum VtError {
    InvalidKey,
    RateLimited(Option<u64>),
    NotFound,
    Network(String),
    Api(String),
}

impl fmt::Display for VtError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VtError::InvalidKey => write!(formatter, "API key inválida"),
            VtError::RateLimited(Some(seconds)) => {
                write!(formatter, "límite de la API; reintenta en {seconds}s")
            }
            VtError::RateLimited(None) => write!(formatter, "límite de la API alcanzado"),
            VtError::NotFound => write!(formatter, "no encontrado"),
            VtError::Network(message) => write!(formatter, "error de red: {message}"),
            VtError::Api(message) => write!(formatter, "error de VirusTotal: {message}"),
        }
    }
}

pub enum Lookup {
    Found(Stats),
    NotFound,
}

pub enum Analysis {
    Pending,
    Completed(Stats),
}

pub struct Client {
    agent: ureq::Agent,
    api_key: Zeroizing<String>,
    limiter: RateLimiter,
}

impl Client {
    pub fn new(api_key: Zeroizing<String>, limiter: RateLimiter) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(120))
            .timeout_write(Duration::from_secs(300))
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .user_agent(concat!("VTDrop/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent,
            api_key,
            limiter,
        }
    }

    /// Hands the limiter back so its history outlives this client.
    pub fn into_limiter(self) -> RateLimiter {
        self.limiter
    }

    pub fn lookup_file(&mut self, sha256: &str) -> Result<Lookup, VtError> {
        let url = format!("{API_BASE}/files/{sha256}");
        let response = match self.get(&url) {
            Ok(response) => response,
            Err(VtError::NotFound) => return Ok(Lookup::NotFound),
            Err(error) => return Err(error),
        };
        let json = read_json(response)?;
        let stats = json
            .pointer("/data/attributes/last_analysis_stats")
            .cloned()
            .ok_or_else(|| VtError::Api("respuesta sin last_analysis_stats".into()))?;
        let stats: Stats = serde_json::from_value(stats)
            .map_err(|error| VtError::Api(format!("estadísticas inválidas: {error}")))?;
        Ok(Lookup::Found(stats))
    }

    /// Uploads a file and returns the analysis id. `progress` receives the
    /// bytes of the file sent so far.
    pub fn upload(
        &mut self,
        path: &Path,
        size: u64,
        progress: Box<dyn FnMut(u64) -> bool + Send>,
    ) -> Result<String, VtError> {
        let url = if size > LARGE_FILE_THRESHOLD {
            let json = self.get(&format!("{API_BASE}/files/upload_url"))?;
            read_json(json)?
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| VtError::Api("upload_url sin URL".into()))?
                .to_owned()
        } else {
            format!("{API_BASE}/files")
        };

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archivo".into());
        let name = sanitize_filename(&name);
        let boundary = boundary();
        let prefix = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        let suffix = format!("\r\n--{boundary}--\r\n").into_bytes();
        let content_length = prefix.len() as u64 + size + suffix.len() as u64;

        let file = File::open(path).map_err(|error| VtError::Api(error.to_string()))?;
        let body = MultipartReader {
            prefix: Cursor::new(prefix),
            file,
            suffix: Cursor::new(suffix),
            sent: 0,
            progress,
        };

        self.limiter.wait();
        let length = content_length.to_string();
        let result = self
            .agent
            .post(&url)
            .set("x-apikey", self.api_key.as_str())
            .set("accept", "application/json")
            .set(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .set("content-length", &length)
            .send(body);
        let response = match result {
            Ok(response) => response,
            Err(error) => return Err(map_ureq_error(error)),
        };
        let json = read_json(response)?;
        json.pointer("/data/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| VtError::Api("subida sin analysis id".into()))
    }

    pub fn analysis(&mut self, id: &str) -> Result<Analysis, VtError> {
        let json = self.get(&format!("{API_BASE}/analyses/{id}"))?;
        let json = read_json(json)?;
        let status = json
            .pointer("/data/attributes/status")
            .and_then(Value::as_str)
            .ok_or_else(|| VtError::Api("análisis sin estado".into()))?;
        if status != "completed" {
            return Ok(Analysis::Pending);
        }
        let stats = json
            .pointer("/data/attributes/stats")
            .cloned()
            .ok_or_else(|| VtError::Api("análisis completado sin estadísticas".into()))?;
        let stats: Stats = serde_json::from_value(stats)
            .map_err(|error| VtError::Api(format!("estadísticas inválidas: {error}")))?;
        Ok(Analysis::Completed(stats))
    }

    fn get(&mut self, url: &str) -> Result<ureq::Response, VtError> {
        // A 429 is usually the per-minute budget: wait as asked and retry
        // twice before surfacing it (the daily quota will still fail).
        let mut retries = 2;
        loop {
            self.limiter.wait();
            let result = self
                .agent
                .get(url)
                .set("x-apikey", self.api_key.as_str())
                .set("accept", "application/json")
                .call();
            match result {
                Err(ureq::Error::Status(429, response)) if retries > 0 => {
                    retries -= 1;
                    let seconds = response
                        .header("retry-after")
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(60)
                        .clamp(1, 120);
                    thread::sleep(Duration::from_secs(seconds));
                }
                result => return result.map_err(map_ureq_error),
            }
        }
    }
}

fn map_ureq_error(error: ureq::Error) -> VtError {
    match error {
        ureq::Error::Status(401, _) => VtError::InvalidKey,
        ureq::Error::Status(403, _) => VtError::Api("acceso denegado (403)".into()),
        ureq::Error::Status(404, _) => VtError::NotFound,
        ureq::Error::Status(429, response) => {
            let seconds = response
                .header("retry-after")
                .and_then(|value| value.parse::<u64>().ok());
            VtError::RateLimited(seconds)
        }
        ureq::Error::Status(status, response) => {
            let detail = response
                .into_string()
                .ok()
                .and_then(|body| {
                    serde_json::from_str::<Value>(&body).ok().and_then(|json| {
                        json.pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                })
                .unwrap_or_else(|| format!("HTTP {status}"));
            VtError::Api(detail)
        }
        ureq::Error::Transport(transport) => VtError::Network(transport.to_string()),
    }
}

fn read_json(response: ureq::Response) -> Result<Value, VtError> {
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE)
        .read_to_end(&mut body)
        .map_err(|error| VtError::Network(error.to_string()))?;
    if body.is_empty() {
        return Err(VtError::Api("respuesta vacía".into()));
    }
    serde_json::from_slice(&body).map_err(|error| VtError::Api(format!("JSON inválido: {error}")))
}

fn boundary() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("----VTDropBoundary{:x}{:x}", std::process::id(), nanos)
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character == '"' || character == '\\' || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect()
}

struct MultipartReader {
    prefix: Cursor<Vec<u8>>,
    file: File,
    suffix: Cursor<Vec<u8>>,
    sent: u64,
    /// Returns false to abort the upload (cancel).
    progress: Box<dyn FnMut(u64) -> bool + Send>,
}

impl Read for MultipartReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.prefix.read(buffer)?;
        if read > 0 {
            return Ok(read);
        }
        let read = self.file.read(buffer)?;
        if read > 0 {
            self.sent += read as u64;
            if !(self.progress)(self.sent) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelado"));
            }
            return Ok(read);
        }
        self.suffix.read(buffer)
    }
}

/// Sliding window: at most `max` requests in any `window`. Unlike spacing
/// every request 15 s apart, a lookup + upload or a few dropped files go out
/// at once while the per-minute quota is still respected.
pub struct RateLimiter {
    max: usize,
    window: Duration,
    recent: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(requests_per_minute: u32) -> Self {
        Self::with_window(requests_per_minute.max(1) as usize, Duration::from_secs(60))
    }

    fn with_window(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            recent: VecDeque::with_capacity(max),
        }
    }

    fn wait(&mut self) {
        loop {
            let now = Instant::now();
            while self
                .recent
                .front()
                .is_some_and(|sent| now.duration_since(*sent) >= self.window)
            {
                self.recent.pop_front();
            }
            match self.recent.front() {
                Some(oldest) if self.recent.len() >= self.max => {
                    thread::sleep(self.window - now.duration_since(*oldest));
                }
                _ => {
                    self.recent.push_back(now);
                    return;
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct _FileReport {
    data: _FileData,
}

#[derive(Deserialize)]
struct _FileData {
    attributes: _FileAttributes,
}

#[derive(Deserialize)]
struct _FileAttributes {
    last_analysis_stats: Stats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_report_stats() {
        let json: Value = serde_json::from_str(
            r#"{
                "data": {
                    "attributes": {
                        "last_analysis_stats": {
                            "malicious": 1,
                            "suspicious": 2,
                            "undetected": 69,
                            "harmless": 0,
                            "timeout": 0
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        let stats: Stats = serde_json::from_value(
            json.pointer("/data/attributes/last_analysis_stats")
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(stats.ratio(), (3, 72));
    }

    #[test]
    fn parses_completed_analysis() {
        let json: Value = serde_json::from_str(
            r#"{
                "data": {
                    "attributes": {
                        "status": "completed",
                        "stats": {
                            "malicious": 0,
                            "suspicious": 0,
                            "undetected": 72,
                            "harmless": 0
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            json.pointer("/data/attributes/status")
                .and_then(Value::as_str),
            Some("completed")
        );
        let stats: Stats =
            serde_json::from_value(json.pointer("/data/attributes/stats").unwrap().clone())
                .unwrap();
        assert_eq!(stats.ratio(), (0, 72));
    }

    #[test]
    fn filename_is_header_safe() {
        assert_eq!(sanitize_filename("a\"b\nc\\d.exe"), "a_b_c_d.exe");
    }

    #[test]
    fn rate_limiter_allows_a_burst_then_waits() {
        let mut limiter = RateLimiter::with_window(2, Duration::from_millis(300));
        let start = Instant::now();
        limiter.wait();
        limiter.wait();
        assert!(start.elapsed() < Duration::from_millis(100));
        limiter.wait();
        assert!(start.elapsed() >= Duration::from_millis(300));
    }
}
