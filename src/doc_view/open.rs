//! `drovr doc open <path> [--title <t>] [--recent] [--focus]`: show a document
//! in the doc pane of the caller's workspace, splitting one off when there is
//! none, and remember it in the workspace's recent documents.
//!
//! Runs on the machine that hosts the workspace (agents, the drovr-docs plugin
//! and the sidebar all call it there), so the control file and the recent
//! store are that machine's.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::client::ApiClient;
use crate::api::schema::{
    Method, PaneListParams, PaneRightClickTarget, PaneSendInputParams, PaneSplitParams, PaneTarget,
    Request, SplitDirection,
};

/// Recent documents kept per workspace.
const RECENT_LIMIT: usize = 20;
/// Share of the caller's width the caller keeps when the doc pane is split off.
const CALLER_RATIO: f32 = 0.55;

const USAGE: &str = "usage: drovr doc open <path> [--title <title>] [--focus]\n       drovr doc open --recent [--focus]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentDoc {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Unix seconds.
    pub opened_at: u64,
}

impl RecentDoc {
    /// Title, or the file name when there is none.
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| {
            self.path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.display().to_string())
        })
    }
}

/// `<state dir>/drovr/docs.json`: workspace id -> recent documents, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentStore {
    #[serde(default)]
    pub workspaces: BTreeMap<String, Vec<RecentDoc>>,
}

impl RecentStore {
    /// Puts `doc` first in the workspace's list, dropping an older entry for
    /// the same path (keeping its title when the new one has none).
    pub fn record(&mut self, workspace_id: &str, mut doc: RecentDoc) {
        let list = self.workspaces.entry(workspace_id.to_owned()).or_default();
        if let Some(index) = list.iter().position(|old| old.path == doc.path) {
            let old = list.remove(index);
            if doc.title.is_none() {
                doc.title = old.title;
            }
        }
        list.insert(0, doc);
        list.truncate(RECENT_LIMIT);
    }

    pub fn recent(&self, workspace_id: &str) -> &[RecentDoc] {
        self.workspaces.get(workspace_id).map_or(&[], Vec::as_slice)
    }
}

pub fn store_path() -> PathBuf {
    crate::config::state_dir().join("drovr").join("docs.json")
}

/// Missing or unreadable store reads as empty.
pub fn load_store(path: &Path) -> RecentStore {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// Read-modify-write under an exclusive lock, so concurrent `doc open` calls
/// (several agents, the sidebar) keep each other's entries. A file that does
/// not parse is moved aside rather than overwritten.
pub fn update_store(path: &Path, change: impl FnOnce(&mut RecentStore)) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(PathBuf::from(lock_path))?;
    lock.lock()?;
    let mut store = match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| {
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".bad-{}", unix_now()));
            let _ = std::fs::rename(path, PathBuf::from(aside));
            RecentStore::default()
        }),
        Err(_) => RecentStore::default(),
    };
    change(&mut store);
    let content = serde_json::to_vec_pretty(&store).map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Absolute, `~`-expanded, lexically normalized path; relative paths are
/// taken from `cwd`.
pub fn resolve_path(arg: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = match (arg.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(arg),
    };
    super::normalize(&cwd.join(expanded))
}

/// The doc pane of a `pane.list` result: a pane of `workspace_id` that
/// reports the `drovr_doc` metadata token.
pub fn find_doc_pane(panes: &Value, workspace_id: &str) -> Option<String> {
    panes["panes"]
        .as_array()?
        .iter()
        .filter(|pane| pane["workspace_id"].as_str() == Some(workspace_id))
        .find(|pane| {
            pane["tokens"][super::METADATA_TOKEN]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        })
        .and_then(|pane| pane["pane_id"].as_str().map(str::to_owned))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OpenArgs {
    path: Option<String>,
    title: Option<String>,
    recent: bool,
    focus: bool,
}

fn parse_args(args: &[String]) -> Result<OpenArgs, String> {
    let mut parsed = OpenArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--title" => {
                parsed.title = Some(iter.next().ok_or("missing value for --title")?.clone())
            }
            "--recent" => parsed.recent = true,
            "--focus" => parsed.focus = true,
            "-h" | "--help" => return Err(String::new()),
            flag if flag.starts_with("--") => return Err(format!("unknown option: {flag}")),
            path if parsed.path.is_none() => parsed.path = Some(path.to_owned()),
            extra => return Err(format!("unexpected argument: {extra}")),
        }
    }
    if parsed.recent == parsed.path.is_some() {
        return Err("give a path or --recent".into());
    }
    Ok(parsed)
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// One API call; the `result` object, or the server's error message.
fn call(client: &ApiClient, id: &str, method: Method) -> io::Result<Value> {
    let request = Request {
        id: format!("drovr:doc:{id}"),
        method,
    };
    let response = client
        .request_value(&request)
        .map_err(|err| io::Error::other(err.to_string()))?;
    if let Some(error) = response.get("error") {
        let message = error["message"].as_str().unwrap_or("request failed");
        return Err(io::Error::other(format!("{id}: {message}")));
    }
    Ok(response["result"].clone())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Entry point for `drovr doc open`; returns the process exit code.
pub fn run_doc_open(args: &[String]) -> io::Result<i32> {
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}");
            }
            eprintln!("{USAGE}");
            return Ok(2);
        }
    };
    let caller = env_var("HERDR_PANE_ID");
    let workspace_env = env_var("HERDR_WORKSPACE_ID");
    if caller.is_none() && workspace_env.is_none() {
        eprintln!("error: drovr doc open runs inside a herdr pane (HERDR_PANE_ID is not set)");
        return Ok(1);
    }
    match open(args, caller, workspace_env) {
        Ok(message) => {
            println!("{message}");
            Ok(0)
        }
        Err(err) => {
            eprintln!("error: {err}");
            Ok(1)
        }
    }
}

fn open(
    args: OpenArgs,
    caller: Option<String>,
    workspace_env: Option<String>,
) -> io::Result<String> {
    let client = ApiClient::local();
    let workspace_id = match workspace_env {
        Some(id) => id,
        None => {
            let pane_id = caller.clone().unwrap_or_default();
            let pane = call(&client, "pane.get", Method::PaneGet(PaneTarget { pane_id }))?;
            pane["pane"]["workspace_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other("cannot find the caller's workspace"))?
        }
    };

    let store = store_path();
    let (path, title) = if args.recent {
        let newest = load_store(&store)
            .recent(&workspace_id)
            .first()
            .cloned()
            .ok_or_else(|| io::Error::other("no recent documents in this workspace"))?;
        (newest.path, newest.title)
    } else {
        let arg = args.path.unwrap_or_default();
        let cwd = std::env::current_dir()?;
        let home = env_var("HOME").map(PathBuf::from);
        (resolve_path(&arg, &cwd, home.as_deref()), args.title)
    };
    if path.is_dir() {
        return Err(io::Error::other(format!(
            "{} is a directory",
            path.display()
        )));
    }
    let doc = RecentDoc {
        path: path.clone(),
        title,
        opened_at: unix_now(),
    };
    if let Err(err) = update_store(&store, |store| store.record(&workspace_id, doc)) {
        tracing::warn!(err = %err, "cannot record recent document");
    }

    let panes = call(
        &client,
        "pane.list",
        Method::PaneList(PaneListParams {
            workspace_id: Some(workspace_id.clone()),
        }),
    )?;
    if let Some(doc_pane) = find_doc_pane(&panes, &workspace_id) {
        let control = super::control_file_path(&doc_pane);
        if let Some(parent) = control.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&control, path.display().to_string())?;
        if args.focus {
            call(
                &client,
                "pane.focus",
                Method::PaneFocus(PaneTarget {
                    pane_id: doc_pane.clone(),
                }),
            )?;
        }
        return Ok(format!("{} in {doc_pane}", path.display()));
    }

    // No doc pane: split one off the caller (or the workspace's focused pane).
    let target = caller
        .filter(|pane_id| {
            panes["panes"].as_array().is_some_and(|list| {
                list.iter()
                    .any(|pane| pane["pane_id"].as_str() == Some(pane_id))
            })
        })
        .or_else(|| {
            let list = panes["panes"].as_array()?;
            list.iter()
                .find(|pane| pane["focused"].as_bool() == Some(true))
                .or_else(|| list.first())
                .and_then(|pane| pane["pane_id"].as_str().map(str::to_owned))
        })
        .ok_or_else(|| io::Error::other("the workspace has no pane to split"))?;
    let split = call(
        &client,
        "pane.split",
        Method::PaneSplit(PaneSplitParams {
            workspace_id: None,
            target_pane_id: Some(target),
            direction: SplitDirection::Right,
            ratio: Some(CALLER_RATIO),
            cwd: path.parent().map(|dir| dir.display().to_string()),
            focus: args.focus,
            right_click: PaneRightClickTarget::default(),
            env: Default::default(),
        }),
    )?;
    let new_pane = split["pane"]["pane_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("pane.split returned no pane id"))?;
    // The viewer only follows later changes of its control file; start it
    // with the current path so an old file left by a reused pane id is inert.
    let control = super::control_file_path(&new_pane);
    if let Some(parent) = control.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&control, path.display().to_string())?;
    let exe = std::env::current_exe()?;
    // `exec`: quitting the viewer closes the pane.
    let command = format!(
        "exec {} doc view {}",
        shell_quote(&exe.display().to_string()),
        shell_quote(&path.display().to_string())
    );
    call(
        &client,
        "pane.send_input",
        Method::PaneSendInput(PaneSendInputParams {
            pane_id: new_pane.clone(),
            text: command,
            keys: vec!["Enter".into()],
        }),
    )?;
    Ok(format!("{} in new pane {new_pane}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn resolves_relative_home_and_absolute_paths() {
        let cwd = Path::new("/work/repo");
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            resolve_path("docs/../plan.md", cwd, home),
            PathBuf::from("/work/repo/plan.md")
        );
        assert_eq!(
            resolve_path("./a.md", cwd, home),
            PathBuf::from("/work/repo/a.md")
        );
        assert_eq!(
            resolve_path("~/notes/a.md", cwd, home),
            PathBuf::from("/home/me/notes/a.md")
        );
        assert_eq!(
            resolve_path("/tmp/x.md", cwd, home),
            PathBuf::from("/tmp/x.md")
        );
        // `~user` is not expanded.
        assert_eq!(
            resolve_path("~bob/a.md", cwd, home),
            PathBuf::from("/work/repo/~bob/a.md")
        );
    }

    #[test]
    fn parses_open_arguments() {
        assert_eq!(
            parse_args(&strings(&["a.md", "--title", "Plan", "--focus"])),
            Ok(OpenArgs {
                path: Some("a.md".into()),
                title: Some("Plan".into()),
                recent: false,
                focus: true,
            })
        );
        assert!(parse_args(&strings(&["--recent"])).unwrap().recent);
        assert!(parse_args(&strings(&[])).is_err());
        assert!(parse_args(&strings(&["a.md", "--recent"])).is_err());
        assert!(parse_args(&strings(&["a.md", "b.md"])).is_err());
        assert!(parse_args(&strings(&["a.md", "--title"])).is_err());
    }

    fn doc(path: &str, title: Option<&str>, at: u64) -> RecentDoc {
        RecentDoc {
            path: PathBuf::from(path),
            title: title.map(str::to_owned),
            opened_at: at,
        }
    }

    #[test]
    fn record_moves_reopened_docs_first_and_caps_the_list() {
        let mut store = RecentStore::default();
        store.record("w1", doc("/a.md", Some("A"), 1));
        store.record("w1", doc("/b.md", None, 2));
        store.record("w1", doc("/a.md", None, 3));
        let list = store.recent("w1");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], doc("/a.md", Some("A"), 3));
        assert_eq!(list[1].label(), "b.md");
        for i in 0..30 {
            store.record("w1", doc(&format!("/{i}.md"), None, 10 + i));
        }
        assert_eq!(store.recent("w1").len(), RECENT_LIMIT);
        assert_eq!(store.recent("w1")[0].path, PathBuf::from("/29.md"));
        assert!(store.recent("w2").is_empty());
    }

    #[test]
    fn store_updates_keep_other_writers_entries_and_set_aside_broken_files() {
        let dir = std::env::temp_dir().join(format!("drovr-docs-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("drovr").join("docs.json");
        update_store(&path, |s| s.record("w1", doc("/a.md", None, 1))).unwrap();
        // Another process writes w2 in between; this writer still sees it.
        update_store(&path, |s| s.record("w2", doc("/b.md", None, 2))).unwrap();
        update_store(&path, |s| s.record("w1", doc("/c.md", None, 3))).unwrap();
        let store = load_store(&path);
        assert_eq!(store.recent("w1").len(), 2);
        assert_eq!(store.recent("w2")[0].path, PathBuf::from("/b.md"));

        std::fs::write(&path, "{ not json").unwrap();
        update_store(&path, |s| s.record("w1", doc("/d.md", None, 4))).unwrap();
        assert_eq!(load_store(&path).recent("w1").len(), 1);
        let aside = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".bad-"));
        assert!(aside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_doc_pane_of_the_workspace() {
        let panes = serde_json::json!({"panes": [
            {"pane_id": "w1:p1", "workspace_id": "w1", "tokens": {"group": "x"}},
            {"pane_id": "w2:p2", "workspace_id": "w2", "tokens": {"drovr_doc": "/a.md"}},
            {"pane_id": "w1:p3", "workspace_id": "w1", "tokens": {"drovr_doc": ""}},
            {"pane_id": "w1:p4", "workspace_id": "w1", "tokens": {"drovr_doc": "/b.md"}},
        ]});
        assert_eq!(find_doc_pane(&panes, "w1").as_deref(), Some("w1:p4"));
        assert_eq!(find_doc_pane(&panes, "w2").as_deref(), Some("w2:p2"));
        assert_eq!(find_doc_pane(&panes, "w3"), None);
        assert_eq!(find_doc_pane(&serde_json::json!({}), "w1"), None);
    }
}
