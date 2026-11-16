//! Data shared between the UI thread and the worker.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub malicious: u32,
    pub suspicious: u32,
    pub undetected: u32,
    pub harmless: u32,
}

impl Stats {
    pub fn detections(self) -> u32 {
        self.malicious.saturating_add(self.suspicious)
    }

    /// Detections / engines that returned a meaningful verdict.
    pub fn ratio(self) -> (u32, u32) {
        (
            self.detections(),
            self.detections()
                .saturating_add(self.undetected)
                .saturating_add(self.harmless),
        )
    }

    pub fn verdict(self, amber: u32, red: u32) -> Verdict {
        let detections = self.detections();
        if detections >= red.max(amber.saturating_add(1)) {
            Verdict::Malicious
        } else if detections >= amber.max(1) {
            Verdict::Suspicious
        } else {
            Verdict::Clean
        }
    }

    /// Worst-first ordering for a batch summary.
    pub fn severity(self) -> (u32, u32) {
        let (found, total) = self.ratio();
        if total == 0 {
            (found, 0)
        } else {
            ((found as u64 * 1_000_000 / total as u64) as u32, found)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    Suspicious,
    Malicious,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub path: PathBuf,
    pub sha256: String,
    pub stats: Stats,
}

impl ScanResult {
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn ratio_text(&self) -> String {
        let (found, total) = self.stats.ratio();
        format!("{found}/{total}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Hashing,
    Querying,
    Uploading,
    Polling,
}

#[derive(Debug, Clone)]
pub struct ErrorInfo {
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    BatchStarted {
        total: usize,
    },
    Progress {
        index: usize,
        total: usize,
        phase: Phase,
    },
    /// Determinate progress of the current hashing/upload step, 0..1.
    Percent(f32),
    /// Files dropped mid-batch were queued; the batch now has `total` files.
    Queued {
        total: usize,
    },
    AskUpload {
        id: u64,
        name: String,
        sha256: String,
        size: u64,
        index: usize,
        total: usize,
    },
    FileResult {
        index: usize,
        total: usize,
        result: ScanResult,
    },
    FileSkipped {
        index: usize,
        total: usize,
        name: String,
    },
    FileError {
        index: usize,
        total: usize,
        error: ErrorInfo,
    },
    BatchDone {
        total: usize,
        results: Vec<ScanResult>,
        errors: usize,
        skipped: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_ignores_engines_without_verdict() {
        let stats = Stats {
            malicious: 1,
            suspicious: 2,
            undetected: 69,
            harmless: 0,
        };
        assert_eq!(stats.ratio(), (3, 72));
        assert_eq!(stats.detections(), 3);
    }

    #[test]
    fn thresholds_default_to_zero_green_one_three_amber_four_red() {
        let mut stats = Stats::default();
        assert_eq!(stats.verdict(1, 4), Verdict::Clean);
        stats.malicious = 1;
        assert_eq!(stats.verdict(1, 4), Verdict::Suspicious);
        stats.malicious = 3;
        assert_eq!(stats.verdict(1, 4), Verdict::Suspicious);
        stats.malicious = 4;
        assert_eq!(stats.verdict(1, 4), Verdict::Malicious);
    }

    #[test]
    fn severity_prefers_larger_ratio_then_absolute_count() {
        let low = Stats {
            malicious: 1,
            undetected: 99,
            ..Stats::default()
        };
        let high = Stats {
            malicious: 4,
            undetected: 6,
            ..Stats::default()
        };
        assert!(high.severity() > low.severity());
    }
}
