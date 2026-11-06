//! Recursive file-name search on a worker thread. Results are streamed back
//! in batches so the view fills in while the walk is still running.

use crate::util;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub type Batch = Vec<(PathBuf, std::fs::Metadata)>;

const MAX_RESULTS: usize = 20_000;

pub fn spawn(root: PathBuf, query: String, show_hidden: bool) -> (async_channel::Receiver<Batch>, Arc<AtomicBool>) {
    let (tx, rx) = async_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        let mut batch = Batch::new();
        let mut last = Instant::now();
        let mut total = 0;
        let walker = walkdir::WalkDir::new(&root)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| show_hidden || !e.file_name().to_string_lossy().starts_with('.'));
        for entry in walker.flatten() {
            if flag.load(Ordering::Relaxed) {
                return;
            }
            if !util::matches_query(&query, &entry.file_name().to_string_lossy()) {
                continue;
            }
            // Use the symlink-following metadata so linked folders show as folders.
            let md = std::fs::metadata(entry.path()).or_else(|_| entry.metadata().map_err(std::io::Error::from));
            let Ok(md) = md else { continue };
            batch.push((entry.into_path(), md));
            total += 1;
            if batch.len() >= 200 || last.elapsed() > Duration::from_millis(150) {
                if tx.send_blocking(std::mem::take(&mut batch)).is_err() {
                    return;
                }
                last = Instant::now();
            }
            if total >= MAX_RESULTS {
                break;
            }
        }
        let _ = tx.send_blocking(batch);
    });
    (rx, cancel)
}
