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
fn ctrl_click_on_a_remote_pane_invokes_the_docs_plugin_there() {
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
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    state.set_endpoint_snapshot(&remote, Box::new(snapshot()));
    assert!(state.activate_endpoint_projection(&remote));
    state.set_pane_surface(md_surface());
    state.compose(106, 20).expect("pane frame");
    let down = mouse(&state, MouseEventKind::Down(MouseButton::Left), "docs");
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    let [ClientShellAction::Endpoint {
        endpoint_id,
        request,
        ..
    }] = &click.actions[..]
    else {
        panic!(
            "expected a plugin action request, got {:?}",
            click.actions.len()
        );
    };
    assert_eq!(endpoint_id, &remote);
    let Method::PluginActionInvoke(params) = &request.method else {
        panic!("expected plugin.action.invoke");
    };
    assert_eq!(params.plugin_id.as_deref(), Some("drovr.docs"));
    assert_eq!(params.action_id, "open-link");
    let context = params.context.as_ref().expect("context");
    assert_eq!(context.clicked_url.as_deref(), Some("/repo/docs/a.md"));
    assert_eq!(context.focused_pane_id.as_deref(), Some("pane_1"));
    assert_eq!(context.workspace_id.as_deref(), Some("ws_1"));
    assert_eq!(context.invocation_source.as_deref(), Some("drovr_click"));
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
