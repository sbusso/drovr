use super::*;
use crate::api::schema::Method;

const LINE: &str = "see docs/a.md ok";

fn md_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(md_surface());
    state.compose(106, 20).expect("pane frame");
    state
}

fn md_surface() -> PaneSurfaceFrame {
    let mut surface = surface();
    surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::with_lines([LINE, "PANE            "]),
        None,
        &[],
    );
    let pane = &mut surface.panes[0];
    pane.rect.width = LINE.len() as u16;
    pane.inner_rect.width = LINE.len() as u16;
    surface
}

fn mouse(state: &ClientShellState, kind: MouseEventKind, needle: &str) -> MouseEvent {
    let pane = &state.hits.panes[0];
    MouseEvent {
        kind,
        column: pane.inner_rect.x + LINE.find(needle).expect("needle") as u16,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::CONTROL,
    }
}

#[test]
fn ctrl_click_on_a_markdown_path_opens_it_locally_and_swallows_the_release() {
    let mut state = md_state();
    let down = mouse(&state, MouseEventKind::Down(MouseButton::Left), "a.md");
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    assert!(matches!(
        &click.actions[..],
        [ClientShellAction::OpenLocalDocument { workspace_id, pane_id, path }]
            if workspace_id == "ws_1" && pane_id == "pane_1" && path == "/repo/docs/a.md"
    ));
    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..down
    };
    let release = state.handle_raw_events(vec![RawInputEvent::Mouse(up)]);
    assert!(release.actions.is_empty() && release.requests.is_empty());
}

#[test]
fn ctrl_click_beside_a_markdown_path_goes_to_the_server_unchanged() {
    let mut state = md_state();
    for needle in ["see", " ok"] {
        let down = mouse(&state, MouseEventKind::Down(MouseButton::Left), needle);
        let click = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
        assert!(
            matches!(
                &click.actions[..],
                [ClientShellAction::Endpoint { request, .. }]
                    if matches!(&request.method, Method::PaneLinkActivate(params)
                        if params.col == down.column - state.hits.panes[0].inner_rect.x)
            ),
            "{needle}"
        );
        state.pending_requests.clear();
        state.url_click_consumes_until_up = false;
    }
}

#[test]
fn ctrl_click_on_a_remote_pane_opens_the_doc_through_the_endpoint_bridge() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let profile = SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("profile id"),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
    };
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(std::slice::from_ref(&profile));
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    state.set_endpoint_snapshot(&remote, Box::new(snapshot()));
    assert!(state.activate_endpoint_projection(&remote));
    state.set_pane_surface(md_surface());
    state.compose(106, 20).expect("pane frame");
    let down = mouse(&state, MouseEventKind::Down(MouseButton::Left), "docs");
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    let [ClientShellAction::OpenRemoteDocument { bridge, doc }] = &click.actions[..] else {
        panic!("expected a bridged doc open, got {:?}", click.actions);
    };
    assert_eq!(bridge.profile(), &profile);
    assert_eq!(
        doc,
        &crate::remote::RemoteDocOpen {
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            pane_id: "pane_1".into(),
            cwd: Some("/repo".into()),
            path: "/repo/docs/a.md".into(),
        }
    );
    assert!(click.requests.is_empty() && state.pending_requests.is_empty());

    // A catalog refresh keeps the bridge while the target and session stay.
    let kept = bridge.clone();
    state.set_endpoint_catalog(std::slice::from_ref(&profile));
    let current = |state: &ClientShellState| {
        state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == remote)
            .and_then(|endpoint| endpoint.bridge.clone())
            .expect("bridge")
    };
    assert!(std::sync::Arc::ptr_eq(&kept, &current(&state)));
    let moved = SavedSshEndpoint {
        target: "dev@other.example".into(),
        ..profile
    };
    state.set_endpoint_catalog(&[moved]);
    assert!(!std::sync::Arc::ptr_eq(&kept, &current(&state)));
}

#[test]
fn ctrl_hover_underlines_a_markdown_path_without_asking_the_server() {
    let mut state = md_state();
    state.set_endpoint_methods(Some(vec!["pane.link.resolve".into()]));
    let hover = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse(
        &state,
        MouseEventKind::Moved,
        "a.md",
    ))]);
    assert!(hover.repaint && hover.actions.is_empty());
    let frame = state.compose(106, 20).expect("hover frame");
    let pane = &state.hits.panes[0];
    let underlined = |col: usize| {
        frame.cells[usize::from(pane.inner_rect.y) * usize::from(frame.width)
            + usize::from(pane.inner_rect.x)
            + col]
            .modifier
            & Modifier::UNDERLINED.bits()
            != 0
    };
    let (start, end) = (LINE.find("docs").unwrap(), LINE.find(" ok").unwrap());
    assert!((start..end).all(underlined));
    assert!(!underlined(start - 1) && !underlined(end));
}

#[test]
fn ctrl_click_on_a_local_relative_path_looks_one_directory_down() {
    let root = std::env::temp_dir().join(format!(
        "drovr-md-click-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let write = |rel: &str| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, "# doc").expect("write");
    };
    let click = |state: &mut ClientShellState| {
        let down = mouse(state, MouseEventKind::Down(MouseButton::Left), "a.md");
        state.handle_raw_events(vec![RawInputEvent::Mouse(down)])
    };
    let cwd = root.to_string_lossy().into_owned();
    let mut fixture = snapshot();
    fixture.panes[0].cwd = Some(cwd.clone());
    fixture.panes[0].foreground_cwd = Some(cwd.clone());

    write("repo-a/docs/a.md");
    let mut state = md_state();
    state.set_snapshot(Box::new(fixture.clone()));
    let opened = click(&mut state);
    assert!(matches!(
        &opened.actions[..],
        [ClientShellAction::OpenLocalDocument { path, .. }]
            if *path == format!("{cwd}/repo-a/docs/a.md")
    ));

    write("repo-b/docs/a.md");
    let mut state = md_state();
    state.set_snapshot(Box::new(fixture));
    let ambiguous = click(&mut state);
    assert!(ambiguous.actions.is_empty());
    let notice = state.visible_endpoint_notice.as_ref().expect("notice");
    assert!(
        notice.body.contains("in 2 subdirectories"),
        "{}",
        notice.body
    );
    std::fs::remove_dir_all(&root).expect("cleanup");
}
