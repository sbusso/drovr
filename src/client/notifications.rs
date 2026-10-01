use std::io;

use tracing::{debug, warn};

use crate::protocol::NotifyKind;

use super::shell;

#[cfg(not(windows))]
use crate::platform::show_desktop_notification as show_untargeted_system_notification;

#[cfg(windows)]
fn queue_system_notification(
    task: impl FnOnce() -> io::Result<bool> + Send + 'static,
) -> io::Result<bool> {
    type Task = Box<dyn FnOnce() -> io::Result<bool> + Send>;
    static QUEUE: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Task>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Task>();
        // Preserve replacement order while keeping native waits off the client loop.
        tokio::spawn(async move {
            while let Some(task) = receiver.recv().await {
                let result = tokio::task::spawn_blocking(task)
                    .await
                    .unwrap_or_else(|err| Err(io::Error::other(err)));
                if let Err(err) = result {
                    warn!(err = %err, "failed to emit system notification");
                }
            }
        });
        sender
    });
    queue.send(Box::new(task)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "notification delivery queue closed",
        )
    })?;
    Ok(true)
}

#[cfg(windows)]
fn show_untargeted_system_notification(title: &str, body: Option<&str>) -> io::Result<bool> {
    let title = title.to_owned();
    let body = body.map(str::to_owned);
    queue_system_notification(move || {
        crate::platform::show_desktop_notification(&title, body.as_deref())
    })
}

pub(super) fn handle_shell_notification_effects(
    effects: Vec<shell::ClientShellNotificationEffect>,
    sound_config: &crate::config::SoundConfig,
    #[cfg(windows)] event_tx: &tokio::sync::mpsc::Sender<super::events::ClientLoopEvent>,
) {
    for effect in effects {
        match effect {
            shell::ClientShellNotificationEffect::Sound { sound, agent } => {
                let agent = agent.as_deref().and_then(crate::detect::parse_agent_label);
                if sound_config.allows(agent) {
                    crate::sound::play(sound, sound_config);
                }
            }
            shell::ClientShellNotificationEffect::Terminal { title, body } => {
                if let Err(err) = crate::terminal_notify::show_notification(&title, body.as_deref())
                {
                    warn!(err = %err, "failed to emit terminal notification");
                }
            }
            shell::ClientShellNotificationEffect::System {
                title,
                body,
                #[cfg(not(windows))]
                click: Some((endpoint_id, pane_id)),
                ..
            } if sheprd_has_display() => {
                sheprd_clickable_notification(title, body, endpoint_id, pane_id)
            }
            shell::ClientShellNotificationEffect::System {
                title,
                body,
                #[cfg(windows)]
                target,
                ..
            } => {
                #[cfg(windows)]
                let result = show_system_notification(&title, body.as_deref(), target, event_tx);
                #[cfg(not(windows))]
                let result = crate::platform::show_desktop_notification(&title, body.as_deref());
                if let Err(err) = result {
                    warn!(err = %err, "failed to emit system notification");
                }
            }
        }
    }
}

#[cfg(windows)]
fn show_system_notification(
    title: &str,
    body: Option<&str>,
    target: Option<shell::ClientSystemNotificationTarget>,
    event_tx: &tokio::sync::mpsc::Sender<super::events::ClientLoopEvent>,
) -> io::Result<bool> {
    let Some(target) = target else {
        return show_untargeted_system_notification(title, body);
    };
    let key = serde_json::to_string(&(
        &target.endpoint_id.storage_key(),
        &target.boot_id,
        &target.pane_id,
    ))
    .map_err(io::Error::other)?;
    let event_tx = event_tx.clone();
    let runtime = tokio::runtime::Handle::current();
    let title = title.to_owned();
    let body = body.map(str::to_owned);
    queue_system_notification(move || {
        crate::platform::show_actionable_desktop_notification(
            &title,
            body.as_deref(),
            key,
            std::sync::Arc::new(move || {
                let event_tx = event_tx.clone();
                let target = target.clone();
                // Never block a native callback: WinRT can dispatch it while Show is awaited.
                runtime.spawn(async move {
                    let _ = event_tx
                        .send(super::events::ClientLoopEvent::NotificationActivated(
                            target,
                        ))
                        .await;
                });
            }),
        )
    })
}

pub(super) fn handle_notify(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
) {
    handle_notify_with_notifiers(
        kind,
        message,
        body,
        sound_config,
        crate::terminal_notify::show_notification,
        show_untargeted_system_notification,
    );
}

pub(super) fn handle_notify_with_notifiers(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
    mut show_terminal_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
    mut show_system_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
) {
    match kind {
        NotifyKind::Sound => {
            let Some(sound) = sound_from_notify_message(message) else {
                warn!(
                    message = message,
                    "received unknown sound notification from server"
                );
                return;
            };
            if sound_config.enabled {
                crate::sound::play(sound, sound_config);
            }
        }
        NotifyKind::Toast => {
            debug!(
                message = message,
                "received terminal toast notification from server"
            );
            if let Err(err) = show_terminal_notification(message, body) {
                warn!(err = %err, "failed to emit terminal notification");
            }
        }
        NotifyKind::SystemToast => {
            debug!(
                message = message,
                "received system toast notification from server"
            );
            if let Err(err) = show_system_notification(message, body) {
                warn!(err = %err, "failed to emit system notification");
            }
        }
    }
}

pub(super) fn sound_from_notify_message(message: &str) -> Option<crate::sound::Sound> {
    match message {
        "agent done" => Some(crate::sound::Sound::Done),
        "agent attention" => Some(crate::sound::Sound::Request),
        _ => None,
    }
}

/// andreconde fork (sheprd): clickable notifications need an X11/Wayland display;
/// without one the effect falls back to a plain notification.
#[cfg(not(windows))]
fn sheprd_has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// andreconde fork (sheprd): a desktop notification you can click. notify-send
/// waits (on its own thread) for the click; clicking raises the terminal running
/// sheprd and focuses the agent. Falls back to a plain notification.
#[cfg(not(windows))]
fn sheprd_clickable_notification(
    title: String,
    body: Option<String>,
    endpoint_id: super::endpoint::ClientEndpointId,
    pane_id: String,
) {
    std::thread::spawn(move || {
        let mut command = std::process::Command::new("notify-send");
        command
            .arg("--app-name")
            .arg("sheprd")
            .arg("--action=default=Open")
            .arg("--wait")
            .arg("--")
            .arg(&title);
        if let Some(body) = body.as_deref().filter(|body| !body.is_empty()) {
            command.arg(body);
        }
        let Ok(output) = command
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            return;
        };
        if String::from_utf8_lossy(&output.stdout).trim() == "default" {
            raise_own_terminal();
            super::shell::request_agent_focus(endpoint_id, pane_id);
        }
    });
}

#[cfg(not(windows))]
/// Focus the terminal window this client runs in (Hyprland): walk up our
/// process tree until a pid matches a Hyprland client window.
fn raise_own_terminal() {
    let Ok(output) = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
    else {
        return;
    };
    let Ok(clients) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
        return;
    };
    let window_pids = clients
        .iter()
        .filter_map(|client| client.get("pid").and_then(serde_json::Value::as_u64))
        .collect::<std::collections::HashSet<_>>();
    let mut pid = u64::from(std::process::id());
    for _ in 0..32 {
        if window_pids.contains(&pid) {
            let _ = std::process::Command::new("hyprctl")
                .args(["dispatch", "focuswindow", &format!("pid:{pid}")])
                .output();
            return;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return;
        };
        // ppid is the 2nd field after the parenthesised command name.
        let Some(ppid) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse::<u64>().ok())
        else {
            return;
        };
        if ppid <= 1 {
            return;
        }
        pid = ppid;
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn native_delivery_keeps_client_responsive_and_preserves_replacement_order() {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        queue_system_notification(move || {
            let _ = started.send(());
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .map_err(io::Error::other)?;
            Ok(true)
        })
        .expect("queued first notification");
        ready.await.expect("notification worker started");
        let (finished, mut second) = tokio::sync::oneshot::channel();
        queue_system_notification(move || {
            let _ = finished.send(());
            Ok(true)
        })
        .expect("queued replacement");
        tokio::task::yield_now().await;
        assert_eq!(
            second.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        // This send must run while native delivery is still waiting.
        release.send(()).expect("client loop remained responsive");
        second.await.expect("replacement delivered after original");
    }
}
