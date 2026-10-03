use super::*;
use crate::api::schema::AgentStatus;
use crate::tasks::{Choice, NewAttempt, NewDecision};

const PROJECT: &str = "Acme";

fn shell() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(super::super::tests::snapshot()));
    state
}

/// A Tasks view on the Acme board, focused.
fn tasks_shell() -> ClientShellState {
    let mut state = shell();
    state.inbox.open = true;
    state.inbox.focused = true;
    state.inbox.view = PanelView::Tasks;
    state.inbox.filter = Some(InboxFilter::Project(PROJECT.into()));
    state.refresh_tasks(true);
    state
}

fn add(title: &str, status: Status, kind: Option<Kind>, criteria: &[&str]) -> String {
    tasks::with_store(|store| {
        store.create_task(
            &NewTask {
                project: PROJECT.into(),
                title: Some(title.into()),
                kind,
                status: Some(status),
                criteria: criteria.iter().map(|c| (*c).to_owned()).collect(),
                ..NewTask::default()
            },
            &Actor::Human,
        )
    })
    .expect("create task")
    .display_id
}

fn start(id: &str, machine: &str, pane: &str) {
    tasks::with_store(|store| {
        store.start_attempt(
            id,
            &NewAttempt {
                harness: "claude".into(),
                machine: machine.into(),
                workspace_key: None,
                pane_key: Some(format!("{machine}/{pane}")),
                session_id: None,
            },
            &Actor::Agent("claude@mato".into()),
        )
    })
    .expect("start attempt");
}

fn render(state: &mut ClientShellState, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    let palette = Palette::catppuccin();
    super::super::inbox::render(
        &mut state.inbox,
        &state.endpoints,
        &palette,
        &mut buffer,
        area,
        false,
        width,
        (0, width),
    );
    buffer
}

fn text_lines(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// A line without the panel border and the left pad.
fn body(line: &str) -> &str {
    line.trim_start_matches('│').trim_start()
}

fn key(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_inbox_key(
        &crate::input::TerminalKey::new(code, KeyModifiers::NONE),
        &mut outcome,
    );
    outcome
}

fn type_text(state: &mut ClientShellState, text: &str) {
    for ch in text.chars() {
        key(state, KeyCode::Char(ch));
    }
}

fn click(state: &mut ClientShellState, (column, row): (u16, u16)) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_inbox_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    outcome
}

fn hit_point(state: &ClientShellState, wanted: &Hit) -> (u16, u16) {
    state
        .inbox
        .tasks
        .hits
        .items
        .iter()
        .find(|(_, hit)| hit == wanted)
        .map(|(rect, _)| (rect.x, rect.y))
        .unwrap_or_else(|| panic!("no hit {wanted:?}"))
}

fn task(id: &str) -> crate::tasks::Task {
    tasks::with_store(|store| store.task(id))
        .expect("read")
        .expect("task exists")
}

#[test]
fn width_60_draws_the_grouped_list_and_width_100_four_columns() {
    let ready = add(
        "Retry the sync job",
        Status::Ready,
        Some(Kind::Fix),
        &["a", "b"],
    );
    let working = add("Attention hook", Status::Working, Some(Kind::Feature), &[]);
    add("Doc pane links", Status::Review, None, &[]);
    let mut state = tasks_shell();

    let lines = text_lines(&render(&mut state, 60, 24));
    assert!(
        lines[0].contains("Inbox  Tasks · Acme") && lines[0].ends_with("+ new"),
        "{lines:?}"
    );
    let ready_at = lines
        .iter()
        .position(|l| body(l).starts_with("Ready 1"))
        .expect("ready lane");
    assert!(
        lines[ready_at + 1].contains(&ready) && lines[ready_at + 1].contains("Retry the sync job")
    );
    assert!(
        lines[ready_at + 2].contains("fix")
            && lines[ready_at + 2].contains("○0/2")
            && lines[ready_at + 2].ends_with("▶ start"),
        "{lines:?}"
    );
    let working_at = lines
        .iter()
        .position(|l| body(l).starts_with("Working 1"))
        .expect("working lane");
    assert!(working_at > ready_at && lines[working_at + 1].contains(&working));
    for lane in ["Triage 0", "Blocked 0", "Review 1", "Done 0"] {
        assert!(lines.iter().any(|l| l.contains(lane)), "{lane}: {lines:?}");
    }

    let lines = text_lines(&render(&mut state, 100, 24));
    let heads = lines
        .iter()
        .find(|l| l.contains("Ready 1"))
        .expect("column heads");
    for lane in ["Working 1", "Blocked 0", "Review 1"] {
        assert!(heads.contains(lane), "{heads}");
    }
    let row = lines
        .iter()
        .find(|l| l.contains(&ready))
        .expect("first card row");
    assert!(row.contains(&working), "cards side by side: {row}");
    assert!(lines.iter().any(|l| body(l).starts_with("Triage 0 ▸")));
    assert!(lines.iter().any(|l| body(l).starts_with("Done 0 ▸")));
}

#[test]
fn the_selected_card_shades_only_its_first_line() {
    let id = add(
        "Spec decision requests",
        Status::Ready,
        Some(Kind::Spec),
        &[],
    );
    let mut state = tasks_shell();
    let buffer = render(&mut state, 60, 20);
    assert_eq!(state.inbox.tasks.selected.as_deref(), Some(id.as_str()));
    let lines = text_lines(&buffer);
    let y = lines.iter().position(|l| l.contains(&id)).expect("card") as u16;
    let palette = Palette::catppuccin();
    // The frame sits in the column left of the text.
    assert_eq!(buffer[(1, y)].symbol(), "╭");
    assert_eq!(buffer[(1, y + 1)].symbol(), "╰");
    assert_eq!(buffer[(10, y)].bg, palette.active_row_bg);
    assert_eq!(buffer[(10, y + 1)].bg, palette.sidebar_bg);
}

#[test]
fn a_card_click_opens_the_view_and_its_button_runs_the_start() {
    let id = add("Retry the sync job", Status::Ready, None, &[]);
    let mut state = tasks_shell();
    render(&mut state, 60, 20);
    let button = hit_point(&state, &Hit::Button(id.clone()));
    click(&mut state, button);
    assert_eq!(state.inbox.tasks.log, [format!("start {id}")]);
    assert!(
        state.inbox.tasks.open.is_none(),
        "the button does not open the view"
    );

    let card = hit_point(&state, &Hit::Card(id.clone()));
    click(&mut state, card);
    assert_eq!(state.inbox.tasks.open.as_deref(), Some(id.as_str()));
    let lines = text_lines(&render(&mut state, 60, 20));
    assert!(
        lines[2].starts_with(&format!("│ ← {id}  [Ready ▾]")),
        "{lines:?}"
    );
    // Esc goes back to the board with the same selection.
    key(&mut state, KeyCode::Esc);
    assert!(state.inbox.tasks.open.is_none());
    assert_eq!(state.inbox.tasks.selected.as_deref(), Some(id.as_str()));
}

#[test]
fn shift_tab_toggles_the_views_and_the_inbox_keeps_its_keys() {
    let mut state = shell();
    state.inbox.open = true;
    state.inbox.focused = true;
    key(&mut state, KeyCode::BackTab);
    assert_eq!(state.inbox.view, PanelView::Tasks);
    key(&mut state, KeyCode::BackTab);
    assert_eq!(state.inbox.view, PanelView::Inbox);
    // Tab still cycles the inbox tabs in the Inbox view.
    let before = state.inbox.tab;
    key(&mut state, KeyCode::Tab);
    assert_ne!(state.inbox.tab, before);
    // With a Tasks input open, shift+tab stays in the input.
    state.inbox.view = PanelView::Tasks;
    state.inbox.filter = Some(InboxFilter::Project(PROJECT.into()));
    state.refresh_tasks(true);
    key(&mut state, KeyCode::Char('n'));
    assert!(state.inbox.tasks.editing());
    key(&mut state, KeyCode::BackTab);
    assert_eq!(state.inbox.view, PanelView::Tasks);
}

#[test]
fn review_to_ready_from_the_move_menu_asks_for_a_note_first() {
    let id = add("Doc pane links", Status::Review, None, &[]);
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.activate_task_menu(
        id.clone(),
        TaskMenu::Move {
            current: Status::Review,
        },
        ClientContextMenuAction::TaskMove(Status::Ready),
        (5, 5),
        &mut outcome,
    );
    assert_eq!(task(&id).status, Status::Review, "nothing written yet");
    assert!(matches!(
        state.inbox.tasks.input.as_ref().map(|i| &i.purpose),
        Some(Purpose::SendBack(_))
    ));
    // Enter on an empty note does nothing.
    key(&mut state, KeyCode::Enter);
    assert_eq!(task(&id).status, Status::Review);
    type_text(&mut state, "links break on wrap");
    key(&mut state, KeyCode::Enter);
    assert_eq!(task(&id).status, Status::Ready);
    assert!(state.inbox.tasks.input.is_none());
    // The move menu marks the current lane.
    let items = task_menu_items(&TaskMenu::Move {
        current: Status::Ready,
    });
    assert_eq!(items.len(), 7);
    assert_eq!(items[1].label, "· Ready");
}

fn decide(id: &str) -> i64 {
    tasks::with_store(|store| {
        store.request_decision(
            id,
            &NewDecision {
                title: "Which table holds decisions?".into(),
                summary: "New table keeps questions simple.".into(),
                choices: vec![
                    Choice {
                        id: "a".into(),
                        label: "New decisions table".into(),
                        consequence: Some("one more migration".into()),
                        recommended: true,
                    },
                    Choice {
                        id: "b".into(),
                        label: "Reuse entries".into(),
                        consequence: None,
                        recommended: false,
                    },
                ],
                allow_text: true,
                default_choice: None,
                expires_at: None,
                wait_until: None,
            },
            &Actor::Agent("claude@mato".into()),
        )
    })
    .expect("decision")
    .id
}

#[test]
fn the_decision_card_lists_choices_and_rules_on_1() {
    let id = add(
        "Spec decision requests",
        Status::Working,
        Some(Kind::Spec),
        &["schema"],
    );
    let decision = decide(&id);
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    let lines = text_lines(&render(&mut state, 60, 30));
    assert!(
        lines.iter().any(|l| l.contains("waiting on you: decision")),
        "{lines:?}"
    );
    let first = lines
        .iter()
        .position(|l| l.contains("? Which table holds decisions?"))
        .expect("card");
    assert!(lines[first].contains('╭'));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("1 New decisions table  (rec) — one more migration")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("2 Reuse entries")));
    assert!(lines.iter().any(|l| l.contains("r reply")));
    key(&mut state, KeyCode::Char('1'));
    let ruled = tasks::with_store(|store| store.decision(decision))
        .expect("read")
        .expect("decision");
    assert_eq!(ruled.ruling_choice.as_deref(), Some("a"));
    assert!(state
        .inbox
        .tasks
        .log
        .contains(&format!("ruled {id}: New decisions table")));
}

#[test]
fn a_data_version_change_reloads_and_a_panel_write_reloads_at_once() {
    let mut state = tasks_shell();
    assert!(state.inbox.tasks.cards.is_empty());
    // Another writer: within 500 ms and at the same data_version, no reload.
    add("Written elsewhere", Status::Triage, None, &[]);
    state.refresh_tasks(false);
    assert!(state.inbox.tasks.cards.is_empty());
    // The data_version moved: the next tick reloads.
    state.inbox.tasks.data_version = Some(i64::MIN);
    state.inbox.tasks.checked = None;
    state.refresh_tasks(false);
    assert_eq!(state.inbox.tasks.cards.len(), 1);
    assert!(state.inbox.tasks.take_changed());
    // A panel write sets `dirty` and reloads without a data_version change.
    key(&mut state, KeyCode::Char('n'));
    type_text(&mut state, "#fix ! Panel task");
    key(&mut state, KeyCode::Enter);
    assert_eq!(state.inbox.tasks.cards.len(), 2);
    let created = state
        .inbox
        .tasks
        .cards
        .iter()
        .find(|card| card.task.title.as_deref() == Some("Panel task"))
        .expect("new card");
    assert_eq!(created.task.kind, Some(Kind::Fix));
    assert_eq!(created.task.priority, Priority::High);
    assert_eq!(created.task.status, Status::Triage);
    assert!(!state.inbox.tasks.dirty);
}

#[test]
fn narrow_panels_fit_and_drop_parts_in_order() {
    let id = add(
        "Attention hook for long running agents in every workspace",
        Status::Working,
        Some(Kind::Feature),
        &["a", "b"],
    );
    start(&id, "mato", "p1");
    start(&id, "mato", "p2"); // the first attempt ends as stopped: outcome `s`
    let mut state = tasks_shell();
    let second_line = |state: &mut ClientShellState, width: u16| {
        let lines = text_lines(&render(state, width, 20));
        let at = lines.iter().position(|l| l.contains(&id)).expect("card");
        (lines[at].clone(), lines[at + 1].clone())
    };
    // 48 columns, inner width 45: id, criteria count and the button glyph.
    let buffer = render(&mut state, 48, 20);
    for y in 2..20 {
        assert_eq!(
            buffer[(47, y)].symbol(),
            " ",
            "the right pad stays empty at row {y}"
        );
    }
    let (first, second) = second_line(&mut state, 48);
    assert!(first.contains(&id) && first.ends_with('…'), "{first}");
    assert!(
        second.contains("○0/2") && second.contains('●') && second.ends_with('↗'),
        "{second}"
    );
    assert!(!second.contains("↗ pane"), "{second}");
    // Wider: everything shows.
    let (_, wide) = second_line(&mut state, 80);
    assert!(
        wide.contains("feature") && wide.contains("claude@mato") && wide.ends_with("↗ pane"),
        "{wide}"
    );
    // Drop order: @machine, kind, outcome, then the agent name.
    for width in 48..90 {
        let (_, line) = second_line(&mut state, width);
        let machine = line.contains("@mato");
        let kind = line.contains("feature");
        let outcome = line.contains(" s ") || line.contains(" s●") || line.contains("  s  ");
        let name = line.contains("claude");
        assert!(!machine || kind, "{width}: {line}");
        assert!(!kind || outcome, "{width}: {line}");
        assert!(!outcome || name, "{width}: {line}");
        assert!(
            line.contains("○0/2") && line.contains('●'),
            "{width}: {line}"
        );
    }
}

#[test]
fn the_header_and_composer_stay_while_the_view_scrolls() {
    let id = add("Long thread", Status::Ready, None, &[]);
    for n in 0..30 {
        tasks::with_store(|store| {
            store.add_entry(
                &id,
                EntryKind::Human,
                &format!("note number {n}"),
                &Actor::Human,
            )
        })
        .expect("note");
    }
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    let before = text_lines(&render(&mut state, 60, 20));
    for _ in 0..6 {
        key(&mut state, KeyCode::Char('j'));
    }
    let after = text_lines(&render(&mut state, 60, 20));
    assert_eq!(before[2], after[2], "header");
    assert!(after[2].contains(&id));
    assert_eq!(before[19], after[19], "composer");
    assert!(after[19].contains("> "), "{after:?}");
    assert_ne!(before[3..19], after[3..19], "the middle scrolled");
}

#[test]
fn a_failed_write_keeps_the_input_text() {
    let mut state = tasks_shell();
    key(&mut state, KeyCode::Char('n'));
    type_text(&mut state, "keep me");
    FAIL_NEXT.with(|fail| fail.set(true));
    key(&mut state, KeyCode::Enter);
    let input = state.inbox.tasks.input.as_ref().expect("input stays open");
    assert_eq!(input.editor.text(), "keep me");
    assert!(state.inbox.tasks.cards.is_empty());
    let lines = text_lines(&render(&mut state, 60, 20));
    assert!(lines[19].contains("tasks db busy"), "{lines:?}");
}

#[test]
fn a_stale_title_keeps_the_input_and_a_second_enter_saves() {
    let id = add("Old title", Status::Ready, None, &[]);
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    render(&mut state, 60, 20);
    let title = hit_point(&state, &Hit::Title);
    click(&mut state, title);
    key(&mut state, KeyCode::Char('!'));
    // An agent edits the task meanwhile.
    tasks::with_store(|store| {
        store.update_task(
            &id,
            &TaskPatch {
                body: Some("agent text".into()),
                ..TaskPatch::default()
            },
            &Actor::Agent("claude@mato".into()),
        )
    })
    .expect("agent edit");
    key(&mut state, KeyCode::Enter);
    assert!(state.inbox.tasks.input.is_some(), "the input stays open");
    assert_eq!(task(&id).title.as_deref(), Some("Old title"));
    assert_eq!(
        state.inbox.tasks.status_text(),
        Some(format!("{id} changed; Enter saves over it").as_str())
    );
    key(&mut state, KeyCode::Enter);
    assert!(state.inbox.tasks.input.is_none());
    assert_eq!(task(&id).title.as_deref(), Some("Old title!"));
}

fn waiting_agent(pane: &str) -> crate::protocol::ClientShellAgent {
    crate::protocol::ClientShellAgent {
        pane_id: pane.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: None,
        agent: Some("claude".into()),
        title: Some("agent".into()),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Blocked,
        state_change_seq: 7,
        state_labels: Vec::new(),
        tokens: [(
            "drovr_wait".to_owned(),
            "permission|ab12cd34||Bash git push".to_owned(),
        )]
        .into_iter()
        .collect(),
        focused: false,
    }
}

#[test]
fn decision_rows_lead_the_waiting_tab_and_task_ids_mark_hook_items() {
    let asked = add("Spec decision requests", Status::Working, None, &[]);
    decide(&asked);
    let live = add("Attention hook", Status::Working, None, &[]);
    let mut state = shell();
    let mut snapshot = super::super::tests::snapshot();
    snapshot.agents = vec![waiting_agent("pane_1")];
    state.set_snapshot(Box::new(snapshot));
    let machine = projects::machine_key(&state.endpoints[0]);
    start(&live, &machine, "pane_1");
    state.inbox.open = true;
    state.inbox.focused = true;
    state.refresh_tasks(true);
    let lines = text_lines(&render(&mut state, 80, 20));
    assert!(
        lines[0].contains("2 waiting"),
        "the decision counts: {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains(&format!("? {asked} Which table holds decisions?")),
        "{lines:?}"
    );
    assert!(
        lines[2].contains(&format!("! {live} ")),
        "task id on the hook item: {lines:?}"
    );
    // j/k move through the decision row first.
    assert!(state.inbox.selected_decision.is_none());
    key(&mut state, KeyCode::Char('k'));
    assert_eq!(
        state.inbox.selected_decision.as_deref(),
        Some(asked.as_str())
    );
    // A click on the row opens the task view on the decision.
    let row = lines[1].find('?').expect("row") as u16;
    click(&mut state, (row, 1));
    assert_eq!(state.inbox.view, PanelView::Tasks);
    assert_eq!(state.inbox.tasks.open.as_deref(), Some(asked.as_str()));
    assert!(state.inbox.tasks.to_decision);
}

#[test]
fn new_task_text_and_store_timestamps_parse() {
    assert_eq!(
        parse_new("#research !! Find the leak"),
        (
            Some(Kind::Research),
            Priority::Urgent,
            "Find the leak".into()
        )
    );
    assert_eq!(
        parse_new("plain words"),
        (None, Priority::Normal, "plain words".into())
    );
    assert_eq!(
        parse_new("#nope text"),
        (None, Priority::Normal, "#nope text".into())
    );
    assert_eq!(unix_of("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(unix_of("2026-10-03T08:15:02Z"), Some(1_791_015_302));
    assert_eq!(unix_of("garbage"), None);
}

#[test]
fn a_sidebar_glyph_and_prefix_a_switch_the_panel_to_the_inbox() {
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_inbox_filtered(InboxFilter::Project(PROJECT.into()), &mut outcome);
    assert_eq!(state.inbox.view, PanelView::Inbox);
    state.inbox.view = PanelView::Tasks;
    state.inbox_oldest_waiting(&mut outcome);
    assert_eq!(state.inbox.view, PanelView::Inbox);
}

#[test]
fn esc_and_the_back_arrow_leave_the_only_task_of_a_workspace() {
    let id = add("Lone task", Status::Ready, None, &[]);
    let mut state = shell();
    let machine = projects::machine_key(&state.endpoints[0]);
    let workspace_key = format!("{machine}/ws_1:lone");
    tasks::with_store(|store| store.link_workspace(&id, Some(&workspace_key)))
        .expect("link workspace");
    state.inbox.open = true;
    state.inbox.focused = true;
    state.inbox.view = PanelView::Tasks;
    state.inbox.filter = Some(InboxFilter::Workspace {
        endpoint_id: state.endpoints[0].endpoint_id.clone(),
        workspace_id: "ws_1".into(),
    });
    state.refresh_tasks(true);
    assert_eq!(state.inbox.tasks.open.as_deref(), Some(id.as_str()));
    key(&mut state, KeyCode::Esc);
    state.refresh_tasks(true);
    assert!(state.inbox.tasks.open.is_none(), "Esc stays on the board");
    // The board reopens the view on Enter; the header arrow leaves it again.
    key(&mut state, KeyCode::Enter);
    assert_eq!(state.inbox.tasks.open.as_deref(), Some(id.as_str()));
    render(&mut state, 80, 20);
    let back = hit_point(&state, &Hit::Back);
    click(&mut state, back);
    state.refresh_tasks(true);
    assert!(state.inbox.tasks.open.is_none(), "← stays on the board");
}

#[test]
fn a_failed_editor_save_after_a_good_one_keeps_the_file() {
    let id = add("Edited", Status::Ready, None, &[]);
    let mut state = tasks_shell();
    let dir = std::env::temp_dir().join(format!("drovr-edit-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, "").expect("write");
    state.inbox.tasks.edit = Some(EditFile {
        id: id.clone(),
        path: path.clone(),
        mtime: None,
        base: task(&id).body,
        saved: false,
        kept: false,
    });
    let write = |text: &str, secs: u64| {
        std::fs::write(&path, text).expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .expect("mtime");
    };
    write("first body\n", 1_000);
    state.poll_task_edit();
    assert_eq!(task(&id).body, "first body");
    write("second body\n", 2_000);
    FAIL_NEXT.with(|fail| fail.set(true));
    state.poll_task_edit();
    assert!(state
        .inbox
        .tasks
        .status_text()
        .is_some_and(|line| line.contains("not saved")));
    state.finish_task_edit();
    assert_eq!(
        std::fs::read_to_string(&path).expect("file kept"),
        "second body\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refused_editor_save_is_reopened_not_overwritten() {
    let id = add("Edited", Status::Ready, None, &[]);
    start(&id, "local", "p1");
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    let dir = std::env::temp_dir().join(format!("drovr-kept-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, "").expect("write");
    state.inbox.tasks.edit = Some(EditFile {
        id: id.clone(),
        path: path.clone(),
        mtime: None,
        base: task(&id).body,
        saved: false,
        kept: false,
    });
    let write = |text: &str, secs: u64| {
        std::fs::write(&path, text).expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .expect("mtime");
    };
    // A version bump that leaves the description alone (a usage copy) does
    // not refuse the save.
    let bump = TaskPatch {
        priority: Some(crate::tasks::Priority::High),
        ..TaskPatch::default()
    };
    tasks::with_store(|store| store.update_task(&id, &bump, &Actor::Human)).expect("bump");
    write("mine\n", 1_000);
    state.poll_task_edit();
    assert_eq!(task(&id).body, "mine");
    // Someone else changes the description: the save is refused and kept.
    let theirs = TaskPatch {
        body: Some("theirs".into()),
        ..TaskPatch::default()
    };
    tasks::with_store(|store| store.update_task(&id, &theirs, &Actor::Human)).expect("theirs");
    write("mine again\n", 2_000);
    state.poll_task_edit();
    assert_eq!(task(&id).body, "theirs");
    // `e` reopens the kept file without writing over it; its save goes in.
    state.refresh_tasks(true);
    let mut outcome = ClientShellInput::default();
    state.edit_description(&mut outcome);
    assert_eq!(
        std::fs::read_to_string(&path).expect("kept"),
        "mine again\n"
    );
    write("mine again\n", 3_000);
    state.poll_task_edit();
    assert_eq!(task(&id).body, "mine again");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_from_a_task_view_goes_back_to_the_board_with_the_input_shown() {
    let id = add("Viewed", Status::Ready, None, &[]);
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    render(&mut state, 80, 20);
    let new = state.inbox.tasks.hits.new;
    click(&mut state, (new.x, new.y));
    assert!(state.inbox.tasks.open.is_none(), "back on the board");
    type_text(&mut state, "seen task");
    let lines = text_lines(&render(&mut state, 80, 20));
    assert!(lines.iter().any(|l| l.contains("seen task")), "{lines:?}");
}

#[test]
fn a_board_input_shows_while_the_board_is_scrolled() {
    let ids: Vec<String> = (0..14)
        .map(|n| add(&format!("Card {n}"), Status::Ready, None, &[]))
        .collect();
    let mut state = tasks_shell();
    state.inbox.tasks.selected = ids.last().cloned();
    state.inbox.tasks.follow = true;
    render(&mut state, 80, 16);
    assert!(
        state.inbox.tasks.scroll > 0,
        "the last card scrolled into view"
    );
    key(&mut state, KeyCode::Char('n'));
    type_text(&mut state, "typed in view");
    let lines = text_lines(&render(&mut state, 80, 16));
    assert!(
        lines.iter().any(|l| l.contains("typed in view")),
        "{lines:?}"
    );
}

#[test]
fn the_chip_close_leaves_the_task_view_too() {
    let id = add("Viewed", Status::Ready, None, &[]);
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    let lines = text_lines(&render(&mut state, 80, 20));
    assert!(lines[1].contains("Acme ✕"), "{lines:?}");
    // The chip starts at the left pad of the line under the header.
    click(&mut state, (2, 1));
    assert!(state.inbox.filter.is_none());
    assert!(state.inbox.tasks.open.is_none());
}

#[test]
fn a_click_away_keeps_the_typed_note_for_the_same_input() {
    let id = add("Noted", Status::Ready, None, &[]);
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.open_task_view(&id, &mut outcome);
    render(&mut state, 80, 24);
    let composer = hit_point(&state, &Hit::Composer);
    click(&mut state, composer);
    type_text(&mut state, "half a note");
    let back = hit_point(&state, &Hit::Back);
    click(&mut state, back);
    assert!(state.inbox.tasks.input.is_none());
    state.open_task_view(&id, &mut outcome);
    render(&mut state, 80, 24);
    let composer = hit_point(&state, &Hit::Composer);
    click(&mut state, composer);
    let text = state
        .inbox
        .tasks
        .input
        .as_ref()
        .map(|input| input.editor.text());
    assert_eq!(text.as_deref(), Some("half a note"));
}

#[test]
fn the_inbox_key_opens_a_closed_panel_on_the_inbox() {
    let mut state = tasks_shell();
    let mut outcome = ClientShellInput::default();
    state.close_inbox(&mut outcome);
    state.toggle_inbox(&mut outcome);
    assert_eq!(state.inbox.view, PanelView::Inbox);
}

#[test]
fn track_makes_a_linked_task_of_a_workspace_without_one() {
    let mut state = shell();
    let machine = projects::machine_key(&state.endpoints[0]);
    state.inbox.open = true;
    state.inbox.focused = true;
    state.inbox.view = PanelView::Tasks;
    state.inbox.filter = Some(InboxFilter::Workspace {
        endpoint_id: state.endpoints[0].endpoint_id.clone(),
        workspace_id: "ws_1".into(),
    });
    state.refresh_tasks(true);
    render(&mut state, 80, 20);
    let track = hit_point(&state, &Hit::Track);
    click(&mut state, track);
    let prefix = format!("{machine}/ws_1:");
    let linked = tasks::with_store(|store| {
        store.list(&TaskFilter {
            workspace_key: Some(prefix.clone()),
            ..TaskFilter::default()
        })
    })
    .expect("list tasks");
    assert_eq!(linked.len(), 1, "one task, linked to the workspace");
    assert!(linked[0]
        .task
        .workspace_key
        .as_deref()
        .is_some_and(|key| key.starts_with(&prefix)));
}

#[test]
fn the_overview_lists_workspaces_and_tracks_one_in_a_click() {
    let mut state = shell();
    state.inbox.open = true;
    state.inbox.focused = true;
    state.inbox.view = PanelView::Tasks;
    state.inbox.filter = None;
    state.refresh_tasks(true);
    let lines = text_lines(&render(&mut state, 60, 12));
    assert!(
        lines.iter().any(|line| line.contains("Other")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("client-shell") && line.contains("+ track")),
        "{lines:#?}"
    );
    let track = hit_point(&state, &Hit::TrackRow(0));
    click(&mut state, track);
    state.inbox.filter = None;
    state.refresh_tasks(true);
    let lines = text_lines(&render(&mut state, 60, 12));
    let tracked = lines
        .iter()
        .find(|line| line.contains("client-shell"))
        .expect("workspace row");
    assert!(tracked.contains("OTH-1"), "{lines:#?}");
}
