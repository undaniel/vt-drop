//! Single worker thread: hashing, VirusTotal requests, upload and polling.
//!
//! The UI thread never blocks. Worker -> UI communication is a mutex-protected
//! queue plus `PostMessage(WM_APP_EVENT)`, so no pointers cross the boundary and
//! the only shared mutable state is explicitly synchronized.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use zeroize::Zeroizing;

use crate::app::WM_APP_EVENT;
use crate::config::Config;
use crate::hash::hash_file_with;
use crate::model::{ErrorInfo, Phase, ScanResult, WorkerEvent};
use crate::vt::{Analysis, Client, Lookup, RateLimiter, VtError};

/// VirusTotal's upload limit (via `upload_url`).
const MAX_UPLOAD: u64 = 650 * 1024 * 1024;

pub enum WorkerCommand {
    Scan(Vec<PathBuf>),
    Decision {
        id: u64,
        accept: bool,
    },
    /// Follows the cancel flag through the channel so the worker knows which
    /// batch it belongs to (anything sent after it is for the next batch).
    Cancel,
    Shutdown,
}

pub struct WorkerHandle {
    sender: mpsc::Sender<WorkerCommand>,
    shutdown: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl WorkerHandle {
    pub fn spawn(
        hwnd: HWND,
        events: Arc<Mutex<VecDeque<WorkerEvent>>>,
        api_key: Option<Zeroizing<String>>,
        config: Config,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            hwnd: hwnd.0 as isize,
            events,
            api_key,
            config,
            shutdown: shutdown.clone(),
            receiver,
            next_id: 1,
            limiter: None,
            queue: VecDeque::new(),
            seen: HashSet::new(),
            index: 0,
            cancel: cancel.clone(),
            stop: false,
            deferred: Vec::new(),
        };
        let _ = thread::Builder::new()
            .name("vtdrop-virus-total".into())
            .spawn(move || worker.run());
        Self {
            sender,
            shutdown,
            cancel,
        }
    }

    pub fn scan(&self, files: Vec<PathBuf>) {
        let _ = self.sender.send(WorkerCommand::Scan(files));
    }

    pub fn decision(&self, id: u64, accept: bool) {
        let _ = self.sender.send(WorkerCommand::Decision { id, accept });
    }

    /// Stops the running batch as soon as possible (hashing and uploads are
    /// aborted mid-read; a request already waiting on the rate limit finishes).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.sender.send(WorkerCommand::Cancel);
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.sender.send(WorkerCommand::Shutdown);
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.sender.send(WorkerCommand::Shutdown);
    }
}

struct Worker {
    hwnd: isize,
    events: Arc<Mutex<VecDeque<WorkerEvent>>>,
    api_key: Option<Zeroizing<String>>,
    config: Config,
    shutdown: Arc<AtomicBool>,
    receiver: mpsc::Receiver<WorkerCommand>,
    next_id: u64,
    /// Kept across batches so the per-minute budget survives dropping the
    /// client (which frees its TLS buffers while idle).
    limiter: Option<RateLimiter>,
    /// Files still to scan in the running batch; drops made while it runs
    /// are appended here instead of waiting for a new batch.
    queue: VecDeque<PathBuf>,
    seen: HashSet<PathBuf>,
    /// Files started in the running batch.
    index: usize,
    /// Set by the UI for an immediate stop (checked while hashing/uploading).
    cancel: Arc<AtomicBool>,
    /// The `Cancel` command was received during this batch.
    stop: bool,
    /// Drops that arrived while a batch was being cancelled; next batch.
    deferred: Vec<PathBuf>,
}

/// An uploaded file waiting for VirusTotal's analysis.
struct Upload {
    index: usize,
    path: PathBuf,
    sha256: String,
    id: String,
    next_check: Instant,
    checks: u32,
}

enum Outcome {
    Done(ScanResult),
    Failed,
    Skipped,
    Uploaded(Upload),
    /// Cancelled or shutting down.
    Stopped,
}

/// Check every 15 s (the public API budget) for up to 10 minutes.
const POLL_EVERY: Duration = Duration::from_secs(15);
const MAX_CHECKS: u32 = 40;

/// Hashing small files is instant; only show a percentage when it is not.
const HASH_PROGRESS_MIN: u64 = 16 * 1024 * 1024;

impl Worker {
    fn run(mut self) {
        loop {
            if !self.deferred.is_empty() {
                let paths = std::mem::take(&mut self.deferred);
                self.process_batch(paths);
                continue;
            }
            match self.receiver.recv() {
                Ok(WorkerCommand::Scan(paths)) => self.process_batch(paths),
                Ok(WorkerCommand::Decision { .. }) => {}
                // The batch it was meant for already ended.
                Ok(WorkerCommand::Cancel) => self.cancel.store(false, Ordering::SeqCst),
                Ok(WorkerCommand::Shutdown) | Err(_) => break,
            }
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
    }

    fn process_batch(&mut self, paths: Vec<PathBuf>) {
        let Some(api_key) = self.api_key.clone() else {
            self.post(WorkerEvent::BatchDone {
                total: 0,
                results: Vec::new(),
                errors: 0,
                skipped: 0,
            });
            return;
        };
        // Keep the HTTP agent (and its TLS buffers) alive only while a batch
        // is running; idle memory stays below the target.
        let limiter = self
            .limiter
            .take()
            .unwrap_or_else(|| RateLimiter::new(self.config.requests_per_minute));
        let mut client = Client::new(api_key, limiter);
        self.process_batch_inner(&mut client, paths);
        self.limiter = Some(client.into_limiter());
    }

    /// Queues files from a `Scan`; anything else that arrives mid-batch is
    /// handled too so nothing is lost.
    fn absorb(&mut self, command: WorkerCommand) {
        match command {
            // Dropped after a cancel: belongs to the next batch, not this one.
            WorkerCommand::Scan(paths) if self.stop || self.cancel.load(Ordering::SeqCst) => {
                self.deferred.extend(paths);
            }
            WorkerCommand::Scan(paths) => {
                let before = self.queue.len();
                self.enqueue(&paths);
                if self.index > 0 && self.queue.len() > before {
                    // Mid-batch: show the bigger counter right away.
                    self.post(WorkerEvent::Queued {
                        total: self.total(),
                    });
                }
            }
            WorkerCommand::Decision { .. } => {}
            WorkerCommand::Cancel => {
                self.stop = true;
                self.cancel.store(false, Ordering::SeqCst);
            }
            WorkerCommand::Shutdown => self.shutdown.store(true, Ordering::SeqCst),
        }
    }

    fn enqueue(&mut self, paths: &[PathBuf]) {
        for file in collect_files(paths, self.config.scan_folders) {
            if self.seen.insert(file.clone()) {
                self.queue.push_back(file);
            }
        }
    }

    fn absorb_pending(&mut self) {
        loop {
            match self.receiver.try_recv() {
                Ok(command) => self.absorb(command),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.shutdown.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.stop || self.cancel.load(Ordering::SeqCst) || self.shutdown.load(Ordering::SeqCst)
    }

    fn total(&self) -> usize {
        self.index + self.queue.len()
    }

    /// Posts at most one `Percent` per whole percent and ~30 per second.
    /// Returns false once the scan was cancelled, which aborts the read.
    fn meter(&self, size: u64) -> impl FnMut(u64) -> bool + Send + 'static {
        let (hwnd, events) = (self.hwnd, self.events.clone());
        let (cancel, shutdown) = (self.cancel.clone(), self.shutdown.clone());
        let mut last = (-1i64, Instant::now());
        move |done: u64| {
            let percent = (done.min(size) * 100 / size.max(1)) as i64;
            if percent != last.0
                && (percent == 100 || last.1.elapsed() >= Duration::from_millis(33))
            {
                last = (percent, Instant::now());
                post_to(hwnd, &events, WorkerEvent::Percent(percent as f32 / 100.0));
            }
            !cancel.load(Ordering::SeqCst) && !shutdown.load(Ordering::SeqCst)
        }
    }

    fn process_batch_inner(&mut self, client: &mut Client, paths: Vec<PathBuf>) {
        self.queue.clear();
        self.seen.clear();
        self.index = 0;
        self.stop = false;
        self.enqueue(&paths);
        if self.queue.is_empty() {
            self.post(WorkerEvent::BatchDone {
                total: 0,
                results: Vec::new(),
                errors: 0,
                skipped: 0,
            });
            return;
        }

        self.post(WorkerEvent::BatchStarted {
            total: self.queue.len(),
        });
        let mut results = Vec::new();
        let mut errors = 0usize;
        let mut skipped = 0usize;
        // Uploads waiting for VirusTotal. They are checked in the background
        // so files dropped meanwhile are looked up right away.
        let mut uploads: Vec<Upload> = Vec::new();

        loop {
            if self.cancelled() {
                break;
            }
            self.absorb_pending();
            if let Some(path) = self.queue.pop_front() {
                self.index += 1;
                match self.scan_file(client, path) {
                    Outcome::Done(result) => results.push(result),
                    Outcome::Failed => errors += 1,
                    Outcome::Skipped => skipped += 1,
                    Outcome::Uploaded(upload) => uploads.push(upload),
                    Outcome::Stopped => break,
                }
                continue;
            }
            let Some(next) = uploads.iter().min_by_key(|upload| upload.next_check) else {
                break;
            };
            let (due, waiting_index) = (next.next_check, next.index);
            self.post(WorkerEvent::Progress {
                index: waiting_index,
                total: self.total(),
                phase: Phase::Polling,
            });
            if !self.wait_until(due) {
                continue; // new files or cancel: handle them first
            }

            let now = Instant::now();
            let mut position = 0;
            while position < uploads.len() && !self.cancelled() {
                if uploads[position].next_check > now {
                    position += 1;
                    continue;
                }
                match client.analysis(&uploads[position].id) {
                    Ok(Analysis::Completed(stats)) => {
                        let upload = uploads.remove(position);
                        let result = ScanResult {
                            path: upload.path,
                            sha256: upload.sha256,
                            stats,
                        };
                        results.push(result.clone());
                        self.post(WorkerEvent::FileResult {
                            index: upload.index,
                            total: self.total(),
                            result,
                        });
                    }
                    Ok(Analysis::Pending) if uploads[position].checks < MAX_CHECKS => {
                        uploads[position].checks += 1;
                        uploads[position].next_check = Instant::now() + POLL_EVERY;
                        position += 1;
                    }
                    outcome => {
                        let upload = uploads.remove(position);
                        errors += 1;
                        let error = match outcome {
                            Err(error) => error_info(error),
                            _ => ErrorInfo {
                                title: "El análisis no terminó".into(),
                                detail: "VirusTotal no completó el análisis en el tiempo máximo de espera."
                                    .into(),
                            },
                        };
                        self.post(WorkerEvent::FileError {
                            index: upload.index,
                            total: self.total(),
                            error,
                        });
                    }
                }
            }
        }

        self.post(WorkerEvent::BatchDone {
            total: self.index,
            results,
            errors,
            skipped,
        });
        if self.cancel.load(Ordering::SeqCst) && !self.stop {
            self.discard_until_cancel();
        }
        self.stop = false;
    }

    /// The UI sets the cancel flag and then sends `Cancel`, so every drop
    /// still in the channel before that command was part of the cancelled
    /// batch: drop it instead of starting a phantom batch with it.
    fn discard_until_cancel(&mut self) {
        while !self.shutdown.load(Ordering::SeqCst) {
            match self.receiver.recv_timeout(Duration::from_secs(2)) {
                Ok(WorkerCommand::Cancel) | Err(mpsc::RecvTimeoutError::Timeout) => break,
                Ok(WorkerCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.shutdown.store(true, Ordering::SeqCst);
                }
                Ok(WorkerCommand::Scan(_) | WorkerCommand::Decision { .. }) => {}
            }
        }
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// Hash, look up and, if needed, ask and upload one file.
    fn scan_file(&mut self, client: &mut Client, path: PathBuf) -> Outcome {
        let index = self.index;
        let total = self.total();
        let failed = |worker: &Self, title: &str, detail: String| {
            worker.post(WorkerEvent::FileError {
                index,
                total,
                error: ErrorInfo {
                    title: title.into(),
                    detail,
                },
            });
            Outcome::Failed
        };
        self.post(WorkerEvent::Progress {
            index,
            total,
            phase: Phase::Hashing,
        });

        let size = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let detail = format!("No se pudo leer «{}»: {error}", display_name(&path));
                return failed(self, "No se pudo leer el archivo", detail);
            }
        };
        let mut meter = (size >= HASH_PROGRESS_MIN).then(|| self.meter(size));
        let cancel = self.cancel.clone();
        let hashed = hash_file_with(&path, |done| match meter.as_mut() {
            Some(meter) => meter(done),
            None => !cancel.load(Ordering::SeqCst),
        });
        let sha256 = match hashed {
            Ok(sha) => sha,
            Err(_) if self.cancelled() => return Outcome::Stopped,
            Err(error) => {
                let detail = format!("No se pudo leer «{}»: {error}", display_name(&path));
                return failed(self, "No se pudo leer el archivo", detail);
            }
        };

        self.post(WorkerEvent::Progress {
            index,
            total,
            phase: Phase::Querying,
        });
        match client.lookup_file(&sha256) {
            Ok(Lookup::Found(stats)) => {
                let result = ScanResult {
                    path,
                    sha256,
                    stats,
                };
                self.post(WorkerEvent::FileResult {
                    index,
                    total,
                    result: result.clone(),
                });
                return Outcome::Done(result);
            }
            Ok(Lookup::NotFound) => {}
            Err(_) if self.cancelled() => return Outcome::Stopped,
            Err(error) => {
                self.post(WorkerEvent::FileError {
                    index,
                    total,
                    error: error_info(error),
                });
                return Outcome::Failed;
            }
        }

        if size > MAX_UPLOAD {
            let detail = format!(
                "«{}» no está en VirusTotal y supera los 650 MB que admite la subida.",
                display_name(&path)
            );
            return failed(self, "Demasiado grande", detail);
        }

        let upload = if self.config.auto_upload {
            true
        } else {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            self.post(WorkerEvent::AskUpload {
                id,
                name: display_name(&path),
                sha256: sha256.clone(),
                size,
                index,
                total,
            });
            match self.wait_for_decision(id) {
                Some(decision) => decision,
                None => return Outcome::Stopped,
            }
        };
        if !upload {
            self.post(WorkerEvent::FileSkipped {
                index,
                total,
                name: display_name(&path),
            });
            return Outcome::Skipped;
        }

        self.post(WorkerEvent::Progress {
            index,
            total,
            phase: Phase::Uploading,
        });
        match client.upload(&path, size, Box::new(self.meter(size))) {
            Ok(id) => Outcome::Uploaded(Upload {
                index,
                path,
                sha256,
                id,
                next_check: Instant::now() + POLL_EVERY,
                checks: 0,
            }),
            Err(_) if self.cancelled() => Outcome::Stopped,
            Err(error) => {
                self.post(WorkerEvent::FileError {
                    index,
                    total,
                    error: upload_error_info(error),
                });
                Outcome::Failed
            }
        }
    }

    /// Waits until `due`, taking drops as they come. Returns false early when
    /// new files are queued or the scan is cancelled.
    fn wait_until(&mut self, due: Instant) -> bool {
        loop {
            if self.cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= due {
                return true;
            }
            match self
                .receiver
                .recv_timeout((due - now).min(Duration::from_millis(200)))
            {
                Ok(command) => {
                    self.absorb(command);
                    if !self.queue.is_empty() {
                        return false;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.shutdown.store(true, Ordering::SeqCst);
                    return false;
                }
            }
        }
    }

    fn wait_for_decision(&mut self, id: u64) -> Option<bool> {
        loop {
            if self.cancelled() {
                return None;
            }
            match self.receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(WorkerCommand::Decision {
                    id: decision_id,
                    accept,
                }) if decision_id == id => return Some(accept),
                Ok(command) => self.absorb(command),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.shutdown.store(true, Ordering::SeqCst);
                    return None;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn post(&self, event: WorkerEvent) {
        post_to(self.hwnd, &self.events, event);
    }
}

fn post_to(hwnd: isize, events: &Mutex<VecDeque<WorkerEvent>>, event: WorkerEvent) {
    if let Ok(mut queue) = events.lock() {
        queue.push_back(event);
    }
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut std::ffi::c_void)),
            WM_APP_EVENT,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

fn collect_files(paths: &[PathBuf], scan_folders: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        collect_one(path, scan_folders, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

fn collect_one(path: &Path, scan_folders: bool, files: &mut Vec<PathBuf>) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    if metadata.is_file() {
        files.push(path.to_path_buf());
        return;
    }
    if !metadata.is_dir() || !scan_folders {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        collect_one(&entry.path(), true, files);
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn error_info(error: VtError) -> ErrorInfo {
    match error {
        VtError::InvalidKey => ErrorInfo {
            title: "API key inválida".into(),
            detail: "VirusTotal rechazó la API key (401). Configúrala desde el menú contextual."
                .into(),
        },
        VtError::RateLimited(Some(seconds)) => ErrorInfo {
            title: "Límite de la API".into(),
            detail: format!(
                "VirusTotal pidió esperar {seconds} segundos antes de volver a consultar."
            ),
        },
        VtError::RateLimited(None) => ErrorInfo {
            title: "Límite de la API".into(),
            detail: "Se alcanzó el límite de peticiones de la API pública.".into(),
        },
        VtError::Network(message) => ErrorInfo {
            title: "Sin conexión".into(),
            detail: format!("No se pudo contactar con VirusTotal: {message}"),
        },
        VtError::NotFound => ErrorInfo {
            title: "No encontrado".into(),
            detail: "VirusTotal no devolvió el recurso solicitado.".into(),
        },
        VtError::Api(message) => ErrorInfo {
            title: "Error de VirusTotal".into(),
            detail: message,
        },
    }
}

fn upload_error_info(error: VtError) -> ErrorInfo {
    let info = error_info(error);
    ErrorInfo {
        title: "No se pudo subir".into(),
        detail: format!("{} {}", info.title, info.detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_during_a_batch_join_the_queue_once() {
        let root = std::env::temp_dir().join("vtdrop-queue-test");
        let _ = std::fs::create_dir_all(&root);
        let (a, b) = (root.join("a.bin"), root.join("b.bin"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut worker = Worker {
            hwnd: 0,
            events: Arc::new(Mutex::new(VecDeque::new())),
            api_key: None,
            config: Config::default(),
            shutdown: Arc::new(AtomicBool::new(false)),
            receiver,
            next_id: 1,
            limiter: None,
            queue: VecDeque::new(),
            seen: HashSet::new(),
            index: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            stop: false,
            deferred: Vec::new(),
        };
        worker.absorb(WorkerCommand::Scan(vec![a.clone()]));
        sender
            .send(WorkerCommand::Scan(vec![a.clone(), b.clone()]))
            .unwrap();
        worker.absorb_pending();
        assert_eq!(worker.queue, VecDeque::from([a, b]));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn drops_after_a_cancel_go_to_the_next_batch() {
        let root = std::env::temp_dir().join("vtdrop-cancel-test");
        let _ = std::fs::create_dir_all(&root);
        let file = root.join("c.bin");
        std::fs::write(&file, b"c").unwrap();
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut worker = Worker {
            hwnd: 0,
            events: Arc::new(Mutex::new(VecDeque::new())),
            api_key: None,
            config: Config::default(),
            shutdown: Arc::new(AtomicBool::new(false)),
            receiver,
            next_id: 1,
            limiter: None,
            queue: VecDeque::new(),
            seen: HashSet::new(),
            index: 1,
            cancel: cancel.clone(),
            stop: false,
            deferred: Vec::new(),
        };
        let handle_like = |s: &mpsc::Sender<WorkerCommand>| {
            cancel.store(true, Ordering::SeqCst);
            s.send(WorkerCommand::Cancel).unwrap();
        };
        handle_like(&sender);
        sender
            .send(WorkerCommand::Scan(vec![file.clone()]))
            .unwrap();
        assert!(worker.cancelled());
        worker.absorb_pending();
        assert!(worker.queue.is_empty());
        assert_eq!(worker.deferred, vec![file]);
        // The flag is consumed with the command, so it cannot leak into the
        // next batch.
        assert!(!cancel.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_recursive_ignores_directories() {
        let root = std::env::temp_dir().join("vtdrop-worker-test");
        let _ = std::fs::create_dir_all(&root);
        let file = root.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let files = collect_files(std::slice::from_ref(&root), false);
        assert!(files.is_empty());
        let files = collect_files(std::slice::from_ref(&root), true);
        assert!(files.contains(&file));
        let _ = std::fs::remove_dir_all(root);
    }
}
