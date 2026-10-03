//! The remote CLI's outbox (docs/design/tasks.md section 6.3).
//!
//! On a machine without the tasks db, `drovr task` writes each op to
//! `R/task-outbox/{pane}/{seq}.json`, rings the client through a pane
//! metadata token, and waits briefly for the client's reply file.
//!
//! ```text
//! R/task-outbox/{pane_id}/epoch          6 random [a-z0-9] chars
//! R/task-outbox/{pane_id}/seq            last allocated seq
//! R/task-outbox/{pane_id}/.lock/         mkdir lock around seq allocation
//! R/task-outbox/{pane_id}/{seq}.json     one OutboxOp, one line
//! R/task-reply/{pane_id}-{epoch}-{seq}.json  OpResult plus "exit"
//! R/task-reply/{pane_id}-d{decision}.json    ruling of a decision
//! R/tasks/{display_id}.json              TaskDetail snapshot
//! R/tasks/{display_id}.md                context file
//! ```

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::ops::{OpResult, OutboxOp, TaskOp, OUTBOX_V};
use super::TaskDetail;

const LOCK_RETRY: Duration = Duration::from_millis(20);
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_STALE: Duration = Duration::from_secs(10);
pub(crate) const REPLY_POLL: Duration = Duration::from_millis(200);

/// `$DROVR_TASK_OUTBOX_DIR`, else `state_dir()/drovr` (the release path is
/// `${XDG_STATE_HOME:-$HOME/.local/state}/herdr/drovr`).
pub(crate) fn root() -> PathBuf {
    match std::env::var_os("DROVR_TASK_OUTBOX_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => crate::config::state_dir().join("drovr"),
    }
}

/// A queued op: the outbox counter it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Queued {
    pub epoch: String,
    pub seq: u64,
}

/// The client's answer to one op (`R/task-reply/...`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Reply {
    #[serde(flatten)]
    pub result: OpResult,
    pub exit: i32,
}

/// Pane ids become path parts; refuse anything that could leave the
/// directory.
pub(crate) fn valid_pane(pane: &str) -> bool {
    !pane.is_empty()
        && pane != "."
        && pane != ".."
        && pane
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

pub(crate) fn pane_dir(root: &Path, pane: &str) -> PathBuf {
    root.join("task-outbox").join(pane)
}

pub(crate) fn reply_path(root: &Path, pane: &str, queued: &Queued) -> PathBuf {
    root.join("task-reply")
        .join(format!("{pane}-{}-{}.json", queued.epoch, queued.seq))
}

pub(crate) fn decision_reply_path(root: &Path, pane: &str, decision_id: i64) -> PathBuf {
    root.join("task-reply")
        .join(format!("{pane}-d{decision_id}.json"))
}

pub(crate) fn snapshot_path(root: &Path, display_id: &str) -> PathBuf {
    root.join("tasks").join(format!("{display_id}.json"))
}

/// Writes `op` as the next file of the pane's outbox.
pub(crate) fn queue(root: &Path, pane: &str, op: &TaskOp) -> io::Result<Queued> {
    if !valid_pane(pane) {
        return Err(io::Error::other(format!("bad pane id {pane:?}")));
    }
    let dir = pane_dir(root, pane);
    std::fs::create_dir_all(&dir)?;
    let queued = {
        let _lock = Lock::take(&dir)?;
        let epoch = match read_trimmed(&dir.join("epoch")) {
            Some(epoch) if !epoch.is_empty() => epoch,
            _ => {
                let epoch = new_epoch();
                write_atomic(&dir.join("epoch"), epoch.as_bytes())?;
                epoch
            }
        };
        let last = read_trimmed(&dir.join("seq"))
            .and_then(|seq| seq.parse::<u64>().ok())
            .unwrap_or(0);
        let seq = last + 1;
        write_atomic(&dir.join("seq"), seq.to_string().as_bytes())?;
        Queued { epoch, seq }
    };
    let line = OutboxOp {
        v: OUTBOX_V,
        epoch: queued.epoch.clone(),
        seq: queued.seq,
        ts: unix_secs(),
        pane: pane.to_owned(),
        op: op.clone(),
    };
    let mut text = serde_json::to_string(&line).map_err(io::Error::other)?;
    text.push('\n');
    let tmp = dir.join(format!(".{}.tmp", queued.seq));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(format!("{}.json", queued.seq)))?;
    Ok(queued)
}

/// Rings the client: `herdr pane report-metadata` with the `drovr_tq`
/// token. A failure is ignored; the client's sweep still finds the file.
pub(crate) fn ring(herdr: &str, pane: &str, queued: &Queued) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let _ = std::process::Command::new(herdr)
        .args([
            "pane",
            "report-metadata",
            pane,
            "--source",
            "drovr-task",
            "--seq",
            &nanos.to_string(),
            "--token",
            &format!("drovr_tq={}.{}|{}", queued.epoch, queued.seq, unix_secs()),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Polls for a JSON file until `timeout`; None when it did not appear or
/// does not parse.
pub(crate) fn wait_json<T: for<'de> Deserialize<'de>>(path: &Path, timeout: Duration) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(value) = serde_json::from_str(&text) {
                return Some(value);
            }
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(REPLY_POLL.min(timeout.saturating_sub(start.elapsed())));
    }
}

/// Every task snapshot the client wrote to this machine.
pub(crate) fn snapshots(root: &Path) -> Vec<TaskDetail> {
    let Ok(read) = std::fs::read_dir(root.join("tasks")) else {
        return Vec::new();
    };
    let mut details: Vec<TaskDetail> = read
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect();
    details.sort_by(|a, b| {
        (a.project.name.as_str(), a.task.number).cmp(&(b.project.name.as_str(), b.task.number))
    });
    details
}

pub(crate) fn snapshot(root: &Path, display_id: &str) -> Option<TaskDetail> {
    let text = std::fs::read_to_string(snapshot_path(root, display_id)).ok()?;
    serde_json::from_str(&text).ok()
}

struct Lock(PathBuf);

impl Lock {
    fn take(dir: &Path) -> io::Result<Lock> {
        let path = dir.join(".lock");
        let start = Instant::now();
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Lock(path)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    if start.elapsed() >= LOCK_WAIT {
                        return Err(io::Error::other(format!(
                            "outbox locked: {}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn new_epoch() -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.write_u32(std::process::id());
    let mut value = hasher.finish();
    (0..6)
        .map(|_| {
            let ch = ALPHABET[(value % ALPHABET.len() as u64) as usize] as char;
            value /= ALPHABET.len() as u64;
            ch
        })
        .collect()
}

pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::ops::parse_outbox_line;
    use crate::tasks::test_support::TempDir;

    fn note(body: &str) -> TaskOp {
        TaskOp::Note {
            task: Some("AC-1".into()),
            body: body.into(),
        }
    }

    #[test]
    fn queue_writes_files_and_counts_up() {
        let dir = TempDir::new("outbox");
        let root = dir.path();
        let first = queue(root, "p1", &note("one")).unwrap();
        let second = queue(root, "p1", &note("two")).unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(first.epoch, second.epoch);
        assert_eq!(first.epoch.len(), 6);
        assert!(first
            .epoch
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
        let pane = pane_dir(root, "p1");
        assert_eq!(std::fs::read_to_string(pane.join("seq")).unwrap(), "2");
        assert_eq!(
            std::fs::read_to_string(pane.join("epoch")).unwrap(),
            first.epoch
        );
        assert!(!pane.join(".lock").exists());
        let line = std::fs::read_to_string(pane.join("2.json")).unwrap();
        assert_eq!(line.lines().count(), 1);
        let parsed = parse_outbox_line(line.trim()).unwrap();
        assert_eq!(parsed.seq, 2);
        assert_eq!(parsed.op, note("two"));

        // Removing op files keeps the counter.
        std::fs::remove_file(pane.join("1.json")).unwrap();
        std::fs::remove_file(pane.join("2.json")).unwrap();
        let third = queue(root, "p1", &note("three")).unwrap();
        assert_eq!((third.epoch.as_str(), third.seq), (first.epoch.as_str(), 3));

        // Removing the directory starts a new epoch at 1.
        std::fs::remove_dir_all(&pane).unwrap();
        let fresh = queue(root, "p1", &note("four")).unwrap();
        assert_eq!(fresh.seq, 1);
        assert_ne!(fresh.epoch, first.epoch);
    }

    #[test]
    fn a_stale_lock_is_removed() {
        let dir = TempDir::new("lock");
        let pane = pane_dir(dir.path(), "p2");
        std::fs::create_dir_all(pane.join(".lock")).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        let lock = std::fs::File::open(pane.join(".lock")).unwrap();
        lock.set_modified(old).unwrap();
        assert_eq!(queue(dir.path(), "p2", &note("x")).unwrap().seq, 1);
    }

    #[test]
    fn bad_pane_ids_are_refused() {
        let dir = TempDir::new("pane");
        for pane in ["", "..", "a/b", "p 1"] {
            assert!(queue(dir.path(), pane, &note("x")).is_err(), "{pane:?}");
        }
    }

    #[test]
    fn wait_json_reads_a_reply() {
        let dir = TempDir::new("reply");
        let queued = Queued {
            epoch: "abc123".into(),
            seq: 4,
        };
        let path = reply_path(dir.path(), "p1", &queued);
        assert!(wait_json::<Reply>(&path, Duration::ZERO).is_none());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"ok":false,"task":"AC-1","status":null,"message":"criteria 2 failed","code":"criteria_failed","decision_id":null,"exit":3}"#,
        )
        .unwrap();
        let reply: Reply = wait_json(&path, Duration::ZERO).unwrap();
        assert_eq!(reply.exit, 3);
        assert_eq!(reply.result.message, "criteria 2 failed");
    }
}
