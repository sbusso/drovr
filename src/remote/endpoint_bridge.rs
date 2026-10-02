//! drovr fork: one bridge per saved remote endpoint. It runs requests on the
//! endpoint's machine for features that a stock herdr server does not offer
//! over the client connection: shell commands over the endpoint's SSH
//! transport (the managed config and its shared master), and herdr API calls
//! through an SSH API bridge socket (`herdr remote-api-bridge`).
//!
//! Remote Ctrl+click doc opens run `drovr doc open` on the remote machine
//! through it. When the SSH shell does not find drovr, the open falls back to
//! the `drovr.docs` plugin's action, invoked through the API bridge; the
//! plugin looks for drovr on the herdr server's PATH.

use std::io;
use std::process::Output;
use std::sync::Mutex;
use std::time::Duration;

use crate::api::client::{ApiClient, ConnectionTarget};
use crate::api::schema::{Method, PluginActionInvokeParams, PluginInvocationContext, Request};
use crate::client::endpoint::SavedSshEndpoint;

use super::saved::{validated_saved_ssh, SavedSshApiBridge};
use super::shell_quote;

/// Plugin and action that run `drovr doc open` on a remote machine.
const DOCS_PLUGIN_ID: &str = "drovr.docs";
const DOCS_PLUGIN_ACTION: &str = "open-link";
/// `invocation_source` that asks the plugin to focus the doc pane.
const DOCS_INVOCATION_SOURCE: &str = "drovr_click";
/// Exit status of the doc open script when the SSH shell does not find drovr.
const NO_DROVR: i32 = 127;
/// Exit status of the doc open script when the path is not a regular file.
const NOT_A_FILE: i32 = 66;
const API_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const PLUGIN_INVOKE_TIMEOUT: Duration = Duration::from_secs(15);

/// A Ctrl+clicked Markdown path in a pane of a remote endpoint. `path` is
/// absolute or `~`-relative; the remote machine expands `~`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteDocOpen {
    pub(crate) workspace_id: String,
    pub(crate) tab_id: String,
    pub(crate) pane_id: String,
    pub(crate) cwd: Option<String>,
    pub(crate) path: String,
}

pub(crate) struct EndpointBridge {
    profile: SavedSshEndpoint,
    /// Started on first use and kept for the life of the client.
    api: Mutex<Option<SavedSshApiBridge>>,
}

impl std::fmt::Debug for EndpointBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointBridge")
            .field("profile", &self.profile.id)
            .finish_non_exhaustive()
    }
}

impl EndpointBridge {
    /// No SSH connection is made until the bridge is used.
    pub(crate) fn new(profile: &SavedSshEndpoint) -> Self {
        Self {
            profile: profile.clone(),
            api: Mutex::new(None),
        }
    }

    pub(crate) fn profile(&self) -> &SavedSshEndpoint {
        &self.profile
    }

    /// Runs a POSIX shell script on the endpoint's machine (15 s limit).
    pub(crate) fn run_sh(&self, script: &str) -> io::Result<Output> {
        let profile = &self.profile;
        validated_saved_ssh(profile.id.as_str(), &profile.target, &profile.session)?
            .sh_output(script)
    }

    /// An API client for the endpoint's herdr server, after a read-only
    /// status probe. A bridge whose metadata went stale is rediscovered
    /// once; other requests are never replayed.
    pub(crate) fn api_client(&self) -> io::Result<ApiClient> {
        let mut api = self
            .api
            .lock()
            .map_err(|_| io::Error::other("endpoint bridge lock poisoned"))?;
        let profile = &self.profile;
        let start = |cached| {
            SavedSshApiBridge::start(
                profile.id.as_str(),
                &profile.target,
                &profile.session,
                cached,
            )
        };
        if api.is_none() {
            *api = Some(start(true)?);
        }
        let probe = |bridge: &SavedSshApiBridge| {
            let client =
                ApiClient::for_target(ConnectionTarget::SocketPath(bridge.socket_path().into()));
            client
                .status_with_timeout(API_PROBE_TIMEOUT)
                .map(|_| client)
                .map_err(|error| {
                    bridge
                        .reported_failure()
                        .unwrap_or_else(|| io::Error::other(error.to_string()))
                })
        };
        let Some(bridge) = api.as_ref() else {
            return Err(io::Error::other("endpoint API bridge unavailable"));
        };
        match probe(bridge) {
            Ok(client) => return Ok(client),
            Err(error) if SavedSshApiBridge::stale_metadata_failure(&error) => {
                bridge.invalidate_metadata();
            }
            Err(error) => {
                *api = None;
                return Err(error);
            }
        }
        *api = None;
        let bridge = start(false)?;
        let client = probe(&bridge)?;
        *api = Some(bridge);
        Ok(client)
    }

    /// Shows `doc` in the doc pane of its workspace on the remote machine and
    /// focuses it: `drovr doc open` there, or the `drovr.docs` plugin when
    /// the SSH shell does not find drovr or the machine runs Windows (no
    /// POSIX shell).
    pub(crate) fn open_document(&self, doc: &RemoteDocOpen) -> io::Result<()> {
        if self.cached_os().as_deref() == Some("windows") {
            return self.open_document_with_plugin(doc);
        }
        let output = self.run_sh(&doc_open_script(&self.profile.session, doc))?;
        match output.status.code() {
            Some(0) => Ok(()),
            Some(NO_DROVR) => self.open_document_with_plugin(doc),
            Some(NOT_A_FILE) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} on {}: not a file", doc.path, self.profile.label),
            )),
            _ => Err(io::Error::other(format!(
                "drovr doc open on {}: {}",
                self.profile.label,
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    /// The OS recorded when the endpoint last connected, if any. Without
    /// it, the POSIX shell is tried first.
    fn cached_os(&self) -> Option<String> {
        let profile = &self.profile;
        crate::client::endpoint::SshMetadataCache::new(
            profile.id.as_str(),
            &profile.target,
            &profile.session,
        )
        .ok()?
        .load()
        .map(|metadata| metadata.os)
    }

    fn open_document_with_plugin(&self, doc: &RemoteDocOpen) -> io::Result<()> {
        let request = Request {
            id: "drovr:doc:plugin".into(),
            method: docs_plugin_method(doc),
        };
        let response = self
            .api_client()?
            .request_value_with_timeout(&request, PLUGIN_INVOKE_TIMEOUT)
            .map_err(|error| io::Error::other(error.to_string()))?;
        match response.get("error") {
            Some(error) => Err(io::Error::other(format!(
                "{DOCS_PLUGIN_ID} on {}: {}",
                self.profile.label,
                error["message"].as_str().unwrap_or("request failed")
            ))),
            None => Ok(()),
        }
    }
}

/// The script `open_document` runs: exit `NOT_A_FILE` unless the path (with
/// `~` expanded) is a regular file, since `drovr doc open` accepts a missing
/// file and waits for it. Then drovr from the PATH, else from `~/.local/bin`
/// (the PATH of a non-interactive SSH shell often lacks it), else exit
/// `NO_DROVR`. The pane and workspace go in the environment, as they do for
/// `drovr doc open` run inside a pane.
fn doc_open_script(session: &str, doc: &RemoteDocOpen) -> String {
    format!(
        "HERDR_PANE_ID={pane}\nHERDR_WORKSPACE_ID={workspace}\nexport HERDR_PANE_ID HERDR_WORKSPACE_ID\nunset HERDR_SOCKET_PATH HERDR_SESSION\ndoc={path}\ncase $doc in \"~\") doc=$HOME ;; \"~/\"*) doc=$HOME/${{doc#\"~/\"}} ;; esac\n[ -f \"$doc\" ] || exit {NOT_A_FILE}\ndrovr=$(command -v drovr 2>/dev/null) || drovr=\"$HOME/.local/bin/drovr\"\n[ -x \"$drovr\" ] || exit {NO_DROVR}\nexec \"$drovr\" --session {session} doc open --focus \"$doc\" </dev/null\n",
        pane = shell_quote(&doc.pane_id),
        workspace = shell_quote(&doc.workspace_id),
        session = shell_quote(session),
        path = shell_quote(&doc.path),
    )
}

fn docs_plugin_method(doc: &RemoteDocOpen) -> Method {
    Method::PluginActionInvoke(PluginActionInvokeParams {
        action_id: DOCS_PLUGIN_ACTION.into(),
        plugin_id: Some(DOCS_PLUGIN_ID.into()),
        context: Some(PluginInvocationContext {
            workspace_id: Some(doc.workspace_id.clone()),
            workspace_label: None,
            workspace_cwd: None,
            worktree: None,
            tab_id: Some(doc.tab_id.clone()),
            tab_label: None,
            focused_pane_id: Some(doc.pane_id.clone()),
            focused_pane_cwd: doc.cwd.clone(),
            focused_pane_agent: None,
            focused_pane_status: None,
            selected_text: None,
            invocation_source: Some(DOCS_INVOCATION_SOURCE.into()),
            correlation_id: None,
            clicked_url: Some(doc.path.clone()),
            link_handler_id: None,
        }),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn doc() -> RemoteDocOpen {
        RemoteDocOpen {
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            pane_id: "w1:p2".into(),
            cwd: Some("/repo".into()),
            path: "~/it's a plan.md".into(),
        }
    }

    fn stub(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            path,
            "#!/bin/sh\n{ printf '%s|' \"$@\"; printf '%s %s\\n' \"$HERDR_PANE_ID\" \"$HERDR_WORKSPACE_ID\"; } > \"$HOME/args\"\n",
        )
        .expect("write stub");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Runs the doc open script as the remote `/bin/sh -s` would, with
    /// `home` as HOME and `path` as PATH.
    fn run(home: &Path, path: &str) -> (Option<i32>, String) {
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(doc_open_script("agents", &doc()))
            .env_clear()
            .env("HOME", home)
            .env("PATH", path)
            .env("HERDR_SOCKET_PATH", "/local/socket")
            .status()
            .expect("run script");
        let args = std::fs::read_to_string(home.join("args")).unwrap_or_default();
        (status.code(), args)
    }

    #[test]
    fn doc_open_script_finds_drovr_on_the_path_then_in_local_bin() {
        let root = std::env::temp_dir().join(format!(
            "drovr-endpoint-bridge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("home");
        let expected = format!(
            "--session|agents|doc|open|--focus|{}/it's a plan.md|w1:p2 w1\n",
            home.display()
        );

        // The file does not exist: drovr is never run.
        stub(&home.join(".local/bin/drovr"));
        assert_eq!(
            run(&home, "/usr/bin:/bin"),
            (Some(NOT_A_FILE), String::new())
        );
        std::fs::remove_file(home.join(".local/bin/drovr")).expect("unstub");
        std::fs::write(home.join("it's a plan.md"), "# plan\n").expect("doc");

        // No drovr anywhere: the plugin fallback's exit status.
        assert_eq!(run(&home, "/usr/bin:/bin"), (Some(NO_DROVR), String::new()));

        stub(&home.join(".local/bin/drovr"));
        assert_eq!(run(&home, "/usr/bin:/bin"), (Some(0), expected.clone()));

        std::fs::remove_file(home.join("args")).expect("reset");
        std::fs::remove_file(home.join(".local/bin/drovr")).expect("unstub");
        let bin = root.join("bin");
        stub(&bin.join("drovr"));
        let path = format!("{}:/usr/bin:/bin", bin.display());
        assert_eq!(run(&home, &path), (Some(0), expected.clone()));

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn plugin_fallback_carries_the_click_context() {
        let Method::PluginActionInvoke(params) = docs_plugin_method(&doc()) else {
            panic!("expected plugin.action.invoke");
        };
        assert_eq!(params.plugin_id.as_deref(), Some("drovr.docs"));
        assert_eq!(params.action_id, "open-link");
        let context = params.context.expect("context");
        assert_eq!(context.clicked_url.as_deref(), Some("~/it's a plan.md"));
        assert_eq!(context.focused_pane_id.as_deref(), Some("w1:p2"));
        assert_eq!(context.workspace_id.as_deref(), Some("w1"));
        assert_eq!(context.tab_id.as_deref(), Some("w1:t1"));
        assert_eq!(context.focused_pane_cwd.as_deref(), Some("/repo"));
        assert_eq!(context.invocation_source.as_deref(), Some("drovr_click"));
    }
}
