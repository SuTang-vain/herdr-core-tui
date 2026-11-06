use std::io;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::layout::{NavDirection, PaneInfo};

use super::discovery::{CoreDiscovery, CoreTerminal};
use super::events::{CoreEvent, CoreEventListener};
use super::menu::{self, MenuAction, Overlay, OverlayOutcome};
use super::session::{CoreRuntimeContext, CoreSession};
use super::store;

struct TerminalRestore {
    active: bool,
}

impl TerminalRestore {
    fn suspend(&mut self) {
        if self.active {
            let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
            ratatui::restore();
            self.active = false;
        }
    }
}

fn init_terminal() -> io::Result<ratatui::DefaultTerminal> {
    let terminal = ratatui::init();
    crossterm::execute!(io::stdout(), EnableMouseCapture)?;
    Ok(terminal)
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        self.suspend();
    }
}

struct ViewLayout {
    content: Rect,
    panes: Vec<PaneInfo>,
}

enum SidebarHit {
    Workspace(usize),
    Pane(crate::layout::PaneId),
}

/// Map a mouse position onto the sidebar rows drawn by `draw`: the `spaces`
/// header, one row per workspace, a blank row, the `panes` header, then two
/// rows (name + state) per visible pane.
fn sidebar_hit(
    session: &CoreSession,
    layout: &ViewLayout,
    frame: Rect,
    column: u16,
    row: u16,
) -> Option<SidebarHit> {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame);
    let columns = Layout::horizontal([Constraint::Length(26), Constraint::Min(1)]).split(rows[1]);
    let sidebar = columns[0];
    if column < sidebar.x || column >= sidebar.x.saturating_add(sidebar.width) {
        return None;
    }
    let inner_top = sidebar.y + 1;
    if row < inner_top || row >= sidebar.y.saturating_add(sidebar.height.saturating_sub(1)) {
        return None;
    }
    let line = usize::from(row - inner_top);
    let workspace_count = session.workspaces.len();
    if line == 0 {
        return None; // "spaces" header
    }
    if line <= workspace_count {
        return Some(SidebarHit::Workspace(line - 1));
    }
    let panes_header = workspace_count + 2; // blank row + "panes" header
    if line <= panes_header || session.active_tab().is_none() {
        return None;
    }
    let pane_line = line - panes_header - 1;
    let index = pane_line / 2; // name row and state row hit the same pane
    layout
        .panes
        .get(index)
        .map(|info| SidebarHit::Pane(info.id))
}

fn hovered_pane(layout: &ViewLayout, column: u16, row: u16) -> Option<crate::layout::PaneId> {
    layout
        .panes
        .iter()
        .find(|info| {
            column >= info.rect.x
                && column < info.rect.x.saturating_add(info.rect.width)
                && row >= info.rect.y
                && row < info.rect.y.saturating_add(info.rect.height)
        })
        .map(|info| info.id)
}

fn view_layout(session: &CoreSession, frame: Rect) -> ViewLayout {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame);
    let columns = Layout::horizontal([Constraint::Length(26), Constraint::Min(1)]).split(rows[1]);
    let content = columns[1];
    let panes = if let Some(tab) = session.active_tab() {
        if tab.zoomed {
            tab.layout
                .panes(content)
                .into_iter()
                .find(|info| info.id == tab.layout.focused())
                .map(|mut info| {
                    info.rect = content;
                    info.inner_rect = content;
                    vec![info]
                })
                .unwrap_or_default()
        } else {
            tab.layout.panes(content)
        }
    } else {
        Vec::new()
    };
    ViewLayout { content, panes }
}

/// Tab strip entries for the header's second row: one per tab plus a
/// trailing "+". `None` marks the new-tab button. Shared by drawing and
/// mouse hit-testing so they never disagree.
fn tab_strip(session: &CoreSession) -> Vec<(String, Option<usize>)> {
    let mut items: Vec<(String, Option<usize>)> = session
        .active_workspace()
        .map(|workspace| {
            workspace
                .tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| {
                    let name = tab.name.as_deref().unwrap_or(&tab.id);
                    (format!(" {name} "), Some(index))
                })
                .collect()
        })
        .unwrap_or_default();
    items.push((" + ".into(), None));
    items
}

/// Hit-test the tab strip row. Returns `Some(Some(tab))` to switch,
/// `Some(None)` for the new-tab button.
fn tab_bar_hit(session: &CoreSession, frame: Rect, column: u16, row: u16) -> Option<Option<usize>> {
    if row != frame.y + 1 {
        return None;
    }
    let mut x = 1u16;
    for (label, action) in tab_strip(session) {
        let width = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        if column >= x && column < x + width {
            return Some(action);
        }
        x += width + 1;
    }
    None
}

/// Active split-border drag. The ratio is recomputed from the mouse
/// position against the split node's total area.
struct SplitDrag {
    path: Vec<bool>,
    direction: Direction,
    area: Rect,
}

fn split_border_hit(
    tab: &super::session::CoreTab,
    content: Rect,
    column: u16,
    row: u16,
) -> Option<SplitDrag> {
    for border in tab.layout.splits(content) {
        let hit = match border.direction {
            Direction::Horizontal => {
                column.abs_diff(border.pos) <= 1
                    && row >= border.area.y
                    && row < border.area.y + border.area.height
            }
            Direction::Vertical => {
                row.abs_diff(border.pos) <= 1
                    && column >= border.area.x
                    && column < border.area.x + border.area.width
            }
        };
        if hit {
            return Some(SplitDrag {
                path: border.path,
                direction: border.direction,
                area: border.area,
            });
        }
    }
    None
}

/// First blocked agent, for a persistent footer reminder while it needs input.
fn blocked_agent(session: &CoreSession) -> Option<String> {
    session
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .flat_map(|tab| tab.panes.values())
        .find(|pane| pane.core.status_state() == Some("blocked"))
        .map(|pane| {
            format!(
                "blocked: {} {} needs an answer",
                pane.core.address,
                pane.core.status_app().unwrap_or("agent")
            )
        })
}

fn core_state_color(state: &str) -> Color {
    match state {
        "working" => Color::Yellow,
        "blocked" | "error" | "missing" => Color::Red,
        "idle" | "done" | "live" => Color::Green,
        _ => Color::DarkGray,
    }
}

fn core_title(pane: &super::session::CorePane) -> String {
    let app = pane
        .core
        .status_app()
        .or(pane.core.agent.as_deref())
        .unwrap_or("shell");
    let state = pane
        .core
        .status_state()
        .or(pane.core.state.as_deref())
        .unwrap_or("unknown");
    format!(" {} · {} · {} ", pane.core.address, app, state)
}

fn draw(
    frame: &mut ratatui::Frame,
    session: &CoreSession,
    layout: &ViewLayout,
    prefix: bool,
    move_pending: bool,
    action_error: Option<&str>,
    startup_notice: Option<&str>,
) {
    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let columns = Layout::horizontal([Constraint::Length(26), Constraint::Min(1)]).split(areas[1]);

    let workspace_name = session
        .active_workspace()
        .and_then(|workspace| workspace.name.as_deref())
        .unwrap_or("workspace");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Herdr",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" · core-native · {workspace_name}")),
        ])),
        areas[0],
    );
    frame.render_widget(
        Paragraph::new("[menu]").style(Style::default().fg(Color::DarkGray)),
        menu::menu_button_area(frame.area()),
    );
    let active_tab = session
        .active_workspace()
        .map(|workspace| workspace.active_tab);
    let mut tab_spans = vec![Span::raw(" ")];
    for (index, (label, action)) in tab_strip(session).into_iter().enumerate() {
        if index > 0 {
            tab_spans.push(Span::raw(" "));
        }
        let style = match action {
            Some(index) if Some(index) == active_tab => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Some(_) => Style::default().fg(Color::Gray),
            None => Style::default().fg(Color::DarkGray),
        };
        tab_spans.push(Span::styled(label, style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)),
        Rect::new(0, frame.area().y + 1, frame.area().width, 1),
    );

    let mut sidebar = Vec::new();
    sidebar.push(Line::from(Span::styled(
        "spaces",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for (index, workspace) in session.workspaces.iter().enumerate() {
        let marker = if index == session.active_workspace {
            "●"
        } else {
            " "
        };
        let name = workspace.name.as_deref().unwrap_or(&workspace.id);
        sidebar.push(Line::from(format!("{marker} {name}")));
    }
    sidebar.push(Line::from(""));
    sidebar.push(Line::from(Span::styled(
        "panes",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if let Some(tab) = session.active_tab() {
        for info in layout.panes.iter().filter_map(|info| {
            tab.layout
                .panes(layout.content)
                .into_iter()
                .find(|candidate| candidate.id == info.id)
        }) {
            let Some(pane) = tab.panes.get(&info.id) else {
                continue;
            };
            let marker = if tab.layout.focused() == info.id {
                "❯"
            } else {
                " "
            };
            sidebar.push(Line::from(format!(
                "{marker} {} {}",
                pane.core.address,
                pane.core.status_app().unwrap_or("shell")
            )));
            let state = pane
                .core
                .status_state()
                .or(pane.core.state.as_deref())
                .unwrap_or("unknown");
            sidebar.push(Line::from(Span::styled(
                format!("  {state}"),
                Style::default().fg(core_state_color(state)),
            )));
        }
    }
    if session
        .active_tab()
        .is_some_and(|tab| tab.zoomed && tab.layout.pane_count() > 1)
    {
        sidebar.push(Line::from(""));
        sidebar.push(Line::from("zoomed"));
    }
    frame.render_widget(
        Paragraph::new(sidebar).block(Block::bordered().title(" core ")),
        columns[0],
    );

    if let Some(tab) = session.active_tab() {
        for info in &layout.panes {
            let Some(pane) = tab.panes.get(&info.id) else {
                continue;
            };
            let focused = info.id == tab.layout.focused();
            let block = Block::bordered()
                .title(core_title(pane))
                .border_style(if focused {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                });
            let inner = block.inner(info.rect);
            frame.render_widget(block, info.rect);
            if let Some(runtime) = &pane.runtime {
                runtime.render(frame, inner, focused);
            } else {
                let state = pane.core.state.as_deref().unwrap_or("unknown");
                let message = match state {
                    "missing" => format!(
                        "core terminal {} is missing\n\nNo replacement was started.\nPress ctrl+b D to remove this pane.",
                        pane.core.address
                    ),
                    "finished" => {
                        let hint = if pane.agent_session.is_some() {
                            "Press Enter to resume the recorded agent session."
                        } else {
                            "No recorded agent session. Enter will not start a new one."
                        };
                        format!("core terminal {} has finished\n\n{hint}", pane.core.address)
                    }
                    _ => format!(
                        "core terminal {} is detached\n\nPress Enter to reattach.",
                        pane.core.address
                    ),
                };
                frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), inner);
            }
        }
    }

    let footer = if let Some(error) = action_error {
        Span::styled(error.to_string(), Style::default().fg(Color::Red))
    } else if let Some(blocked) = blocked_agent(session) {
        Span::styled(
            blocked,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if let Some(notice) = startup_notice {
        // Informational, not an error: yellow and dismissed by first input.
        Span::styled(notice.to_string(), Style::default().fg(Color::Yellow))
    } else if move_pending {
        Span::styled(
            "move: choose a direction",
            Style::default().fg(Color::Yellow),
        )
    } else if prefix {
        Span::styled(
            "prefix: v/- split · x close · a adopt · D drop dead · c/N new · q detach",
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled(
            "ctrl+b prefix · enter reattach detached pane · direct herdr-core",
            Style::default().add_modifier(Modifier::DIM),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(footer)), areas[2]);
}

fn reload_config(
    current_config: &mut crate::config::Config,
    action_error: &mut Option<String>,
    startup_notice: &mut Option<String>,
) {
    let loaded = crate::config::Config::load();
    if !loaded.diagnostics.is_empty() {
        *action_error = Some(format!(
            "config reload reported: {}",
            loaded.diagnostics.join("; ")
        ));
        return;
    }
    crate::core_backend::configure(loaded.config.terminal.backend);
    *current_config = loaded.config;
    *action_error = None;
    *startup_notice = Some("config reloaded".into());
}

/// Shared by ctrl+b a and the menu's adopt action.
fn adopt_flow(
    discovery: &CoreDiscovery,
    discovered: &[CoreTerminal],
    session: &mut CoreSession,
    terminal: &mut ratatui::DefaultTerminal,
    restore: &mut TerminalRestore,
    action_error: &mut Option<String>,
) -> io::Result<()> {
    let terminals = discovery.discover().unwrap_or_else(|_| discovered.to_vec());
    let mut candidates: Vec<CoreTerminal> = terminals
        .into_iter()
        .filter(|core| {
            core.state.as_deref() == Some("live") && !session.has_core_terminal(&core.address)
        })
        .collect();
    candidates.sort_by(|a, b| a.address.cmp(&b.address));
    restore.suspend();
    let choice = super::ui::pick_adopt(&candidates);
    *terminal = init_terminal()?;
    restore.active = true;
    match choice {
        Ok(Some(core)) => {
            if let Err(err) = session.adopt_terminal(discovery, core) {
                *action_error = Some(err.to_string());
            }
        }
        Ok(None) => {}
        Err(err) => *action_error = Some(err.to_string()),
    }
    Ok(())
}

fn save_session(session: &CoreSession, action_error: &mut Option<String>) {
    if let Err(err) = session.save() {
        *action_error = Some(format!("could not save core-native layout: {err}"));
    }
}

fn direction_for_key(key: KeyCode) -> Option<NavDirection> {
    match key {
        KeyCode::Left | KeyCode::Char('h') => Some(NavDirection::Left),
        KeyCode::Right | KeyCode::Char('l') => Some(NavDirection::Right),
        KeyCode::Up | KeyCode::Char('k') => Some(NavDirection::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(NavDirection::Down),
        _ => None,
    }
}

fn open_browser(
    discovery: &CoreDiscovery,
    terminals: Vec<CoreTerminal>,
    terminal: &mut ratatui::DefaultTerminal,
    restore: &mut TerminalRestore,
) -> io::Result<()> {
    restore.suspend();
    let _ = super::ui::run(discovery.clone(), terminals);
    *terminal = init_terminal()?;
    restore.active = true;
    Ok(())
}

/// Apply an overlay outcome. Returns `Ok(true)` when the client should exit.
fn apply_overlay_outcome(
    outcome: OverlayOutcome,
    overlay: &mut Option<Overlay>,
    session: &mut CoreSession,
    discovery: &CoreDiscovery,
    discovered: &[CoreTerminal],
    terminal: &mut ratatui::DefaultTerminal,
    restore: &mut TerminalRestore,
    action_error: &mut Option<String>,
    startup_notice: &mut Option<String>,
    current_config: &mut crate::config::Config,
) -> io::Result<bool> {
    match outcome {
        OverlayOutcome::Keep => {}
        OverlayOutcome::Close => *overlay = None,
        OverlayOutcome::Run(action) => {
            *overlay = None;
            match action {
                MenuAction::Settings => *overlay = Some(Overlay::Settings),
                MenuAction::Keybinds => *overlay = Some(Overlay::Keybinds),
                MenuAction::ReloadConfig => {
                    reload_config(current_config, action_error, startup_notice)
                }
                MenuAction::RenameWorkspace => {
                    let current = session
                        .active_workspace()
                        .and_then(|workspace| workspace.name.clone())
                        .unwrap_or_default();
                    *overlay = Some(Overlay::Rename {
                        workspace: true,
                        input: current,
                    });
                }
                MenuAction::RenameTab => {
                    let current = session
                        .active_tab()
                        .and_then(|tab| tab.name.clone())
                        .unwrap_or_default();
                    *overlay = Some(Overlay::Rename {
                        workspace: false,
                        input: current,
                    });
                }
                MenuAction::Adopt => {
                    adopt_flow(
                        discovery,
                        discovered,
                        session,
                        terminal,
                        restore,
                        action_error,
                    )?;
                    save_session(session, action_error);
                }
                MenuAction::Detach => {
                    save_session(session, action_error);
                    return Ok(true);
                }
                MenuAction::EditConfig => {
                    restore.suspend();
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                    let mut parts = editor.split_whitespace();
                    let status = if let Some(program) = parts.next() {
                        std::process::Command::new(program)
                            .args(parts)
                            .arg(crate::config::config_path())
                            .status()
                    } else {
                        Err(io::Error::other("EDITOR is empty"))
                    };
                    *terminal = init_terminal()?;
                    restore.active = true;
                    match status {
                        Ok(status) if status.success() => {
                            reload_config(current_config, action_error, startup_notice)
                        }
                        Ok(status) => *action_error = Some(format!("editor exited with {status}")),
                        Err(err) => *action_error = Some(format!("could not run $EDITOR: {err}")),
                    }
                }
            }
        }
        OverlayOutcome::SubmitRename { workspace, name } => {
            *overlay = None;
            let result = if workspace {
                session.rename_active_workspace(discovery, name)
            } else {
                session.rename_active_tab(discovery, name)
            };
            if let Err(err) = result {
                *action_error = Some(err.to_string());
            }
            save_session(session, action_error);
        }
    }
    Ok(false)
}

pub(crate) fn run(discovery: CoreDiscovery, discovered: Vec<CoreTerminal>) -> io::Result<()> {
    let (context, mut events) = CoreRuntimeContext::new();
    let render_dirty = context.render_dirty();
    let snapshot = store::load()?;
    let discovered = discovery.enrich(discovered);
    let mut session =
        CoreSession::restore_or_create(snapshot, &discovery, context, discovered.clone())?;
    let (mut _core_listener, mut core_events) = CoreEventListener::spawn(&discovery)?;
    // When the stream drops, retry with capped exponential backoff instead of
    // leaving the UI frozen on the last known state.
    let mut reconnect: Option<(std::time::Instant, std::time::Duration)> = None;
    let mut terminal = init_terminal()?;
    let mut restore = TerminalRestore { active: true };
    let mut prefix = false;
    let mut move_pending = false;
    let mut startup_notice = if session.notices.is_empty() {
        None
    } else {
        Some(session.notices.join("\n"))
    };
    let mut action_error = None;
    let mut overlay: Option<Overlay> = None;
    let mut split_drag: Option<SplitDrag> = None;
    let mut current_config = crate::config::Config::load().config;
    save_session(&session, &mut action_error);

    let size = terminal.size()?;
    let mut layout = view_layout(&session, Rect::new(0, 0, size.width, size.height));
    let mut needs_draw = true;
    loop {
        if let Some((when, backoff)) = reconnect {
            if std::time::Instant::now() >= when {
                match CoreEventListener::spawn(&discovery) {
                    Ok((listener, receiver)) => {
                        _core_listener = listener; // RAII: dropping the old listener kills its child
                        core_events = receiver;
                        reconnect = None;
                        session.refresh_core(&discovery);
                        startup_notice = Some("core event stream reconnected".into());
                        needs_draw = true;
                    }
                    Err(_) => {
                        let next = (backoff * 2).min(std::time::Duration::from_secs(30));
                        reconnect = Some((std::time::Instant::now() + next, next));
                    }
                }
            }
        }
        while let Ok(event) = events.try_recv() {
            session.handle_event(event);
            needs_draw = true;
        }
        while let Ok(event) = core_events.try_recv() {
            match event {
                CoreEvent::Disconnected(error) => {
                    action_error = Some(format!("{error}; reconnecting"));
                    reconnect.get_or_insert((
                        std::time::Instant::now() + std::time::Duration::from_secs(2),
                        std::time::Duration::from_secs(2),
                    ));
                    needs_draw = true;
                }
                CoreEvent::Heartbeat => {}
                CoreEvent::ResyncNeeded => {
                    session.refresh_core(&discovery);
                    needs_draw = true;
                }
                event => {
                    if session.apply_core_event(event) {
                        needs_draw = true;
                    }
                }
            }
        }
        if render_dirty.is_pending() {
            render_dirty.take();
            needs_draw = true;
        }
        if needs_draw {
            let size = terminal.size()?;
            layout = view_layout(&session, Rect::new(0, 0, size.width, size.height));
            for info in &layout.panes {
                let inner = Block::bordered().inner(info.rect);
                session.resize_pane(info.id, inner.height.max(1), inner.width.max(1));
            }
            terminal.draw(|frame| {
                draw(
                    frame,
                    &session,
                    &layout,
                    prefix,
                    move_pending,
                    action_error.as_deref(),
                    startup_notice.as_deref(),
                );
                if let Some(current) = &overlay {
                    menu::draw(frame, current, &menu::settings_lines(&current_config));
                }
            })?;
            needs_draw = false;
        }

        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        needs_draw = true;
        let input_event = event::read()?;
        if matches!(
            input_event,
            Event::Key(_) | Event::Mouse(_) | Event::Paste(_)
        ) {
            // The startup notice is informational; any real input dismisses it.
            startup_notice = None;
        }
        match input_event {
            Event::Key(key) => {
                if let Some(current) = overlay.as_mut() {
                    if key.kind == KeyEventKind::Press {
                        let outcome = current.handle_key(key.code);
                        if apply_overlay_outcome(
                            outcome,
                            &mut overlay,
                            &mut session,
                            &discovery,
                            &discovered,
                            &mut terminal,
                            &mut restore,
                            &mut action_error,
                            &mut startup_notice,
                            &mut current_config,
                        )? {
                            return Ok(());
                        }
                    }
                    continue;
                }
                if move_pending {
                    move_pending = false;
                    if let Some(direction) = direction_for_key(key.code) {
                        session.move_direction(direction, layout.content);
                        save_session(&session, &mut action_error);
                    }
                    continue;
                }
                if key.kind != KeyEventKind::Press {
                    if !prefix {
                        session.send_key(key)?;
                    }
                    continue;
                }
                if prefix {
                    prefix = false;
                    let action = match key.code {
                        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            KeyCode::Char(character.to_ascii_uppercase())
                        }
                        code => code,
                    };
                    match action {
                        KeyCode::Char('q') => {
                            save_session(&session, &mut action_error);
                            return Ok(());
                        }
                        KeyCode::Char('D') => {
                            match session.remove_focused_missing() {
                                Ok(outcome) => {
                                    if outcome.exit_client {
                                        save_session(&session, &mut action_error);
                                        return Ok(());
                                    }
                                }
                                Err(err) => action_error = Some(err.to_string()),
                            }
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('a') => {
                            adopt_flow(
                                &discovery,
                                &discovered,
                                &mut session,
                                &mut terminal,
                                &mut restore,
                                &mut action_error,
                            )?;
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('v') => {
                            if let Err(err) = session.split(&discovery, Direction::Horizontal) {
                                action_error = Some(err.to_string());
                            }
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('-') => {
                            if let Err(err) = session.split(&discovery, Direction::Vertical) {
                                action_error = Some(err.to_string());
                            }
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('x') => {
                            let outcome = session.close_focused(&discovery)?;
                            if !outcome.closed {
                                action_error = Some("no pane was closed".into());
                            }
                            save_session(&session, &mut action_error);
                            if outcome.exit_client {
                                return Ok(());
                            }
                        }
                        KeyCode::Char('z') => {
                            session.toggle_zoom();
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('c') => {
                            if let Err(err) = session.add_tab(&discovery) {
                                action_error = Some(err.to_string());
                            }
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('N') => {
                            if let Err(err) =
                                session.add_workspace(&discovery, std::env::current_dir()?)
                            {
                                action_error = Some(err.to_string());
                            }
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('n') => {
                            session.next_tab(1);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('p') => {
                            session.next_tab(-1);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char(']') => {
                            session.next_workspace(1);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('[') => {
                            session.next_workspace(-1);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('r') => session.refresh_core(&discovery),
                        KeyCode::Char('g') => overlay = Some(Overlay::menu()),
                        KeyCode::Char('m') => move_pending = true,
                        KeyCode::Char('b') => {
                            save_session(&session, &mut action_error);
                            let terminals =
                                discovery.discover().unwrap_or_else(|_| discovered.clone());
                            open_browser(&discovery, terminals, &mut terminal, &mut restore)?;
                            session.refresh_core(&discovery);
                        }
                        KeyCode::Enter => {
                            if let Err(err) = session.restore_focused(&discovery) {
                                action_error = Some(err.to_string());
                            }
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('H') => {
                            session.swap_direction(NavDirection::Left, layout.content);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('L') => {
                            session.swap_direction(NavDirection::Right, layout.content);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('K') => {
                            session.swap_direction(NavDirection::Up, layout.content);
                            save_session(&session, &mut action_error);
                        }
                        KeyCode::Char('J') => {
                            session.swap_direction(NavDirection::Down, layout.content);
                            save_session(&session, &mut action_error);
                        }
                        code => {
                            if let Some(direction) = direction_for_key(code) {
                                session.focus_direction(direction, layout.content);
                                save_session(&session, &mut action_error);
                            }
                        }
                    }
                    continue;
                }
                if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    prefix = true;
                    continue;
                }
                if key.code == KeyCode::Enter
                    && session
                        .focused_pane()
                        .is_some_and(|pane| pane.runtime.is_none())
                {
                    if let Err(err) = session.restore_focused(&discovery) {
                        action_error = Some(err.to_string());
                    }
                    save_session(&session, &mut action_error);
                    continue;
                }
                session.send_key(key)?;
            }
            Event::Paste(text) => session.send_paste(text)?,
            Event::Mouse(mouse) => {
                let size = terminal.size()?;
                let frame = Rect::new(0, 0, size.width, size.height);
                if let Some(current) = overlay.as_mut() {
                    if let Some(outcome) = current.handle_mouse(mouse, frame) {
                        if apply_overlay_outcome(
                            outcome,
                            &mut overlay,
                            &mut session,
                            &discovery,
                            &discovered,
                            &mut terminal,
                            &mut restore,
                            &mut action_error,
                            &mut startup_notice,
                            &mut current_config,
                        )? {
                            return Ok(());
                        }
                    }
                    continue;
                }
                if menu::menu_button_hit(frame, mouse) {
                    overlay = Some(Overlay::menu());
                    continue;
                }
                if mouse.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) {
                    match tab_bar_hit(&session, frame, mouse.column, mouse.row) {
                        Some(Some(index)) => {
                            if let Some(workspace) = session.active_workspace_mut() {
                                if index < workspace.tabs.len() {
                                    workspace.active_tab = index;
                                }
                            }
                            save_session(&session, &mut action_error);
                            continue;
                        }
                        Some(None) => {
                            if let Err(err) = session.add_tab(&discovery) {
                                action_error = Some(err.to_string());
                            }
                            save_session(&session, &mut action_error);
                            continue;
                        }
                        None => {}
                    }
                }
                match mouse.kind {
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        match sidebar_hit(&session, &layout, frame, mouse.column, mouse.row) {
                            Some(SidebarHit::Workspace(index)) => {
                                if index < session.workspaces.len() {
                                    session.active_workspace = index;
                                    save_session(&session, &mut action_error);
                                }
                            }
                            Some(SidebarHit::Pane(pane_id)) => {
                                if let Some(tab) = session.active_tab_mut() {
                                    tab.layout.focus_pane(pane_id);
                                }
                                save_session(&session, &mut action_error);
                            }
                            None => {
                                let border = session.active_tab().and_then(|tab| {
                                    split_border_hit(tab, layout.content, mouse.column, mouse.row)
                                });
                                if let Some(drag) = border {
                                    split_drag = Some(drag);
                                } else if let Some(pane_id) =
                                    hovered_pane(&layout, mouse.column, mouse.row)
                                {
                                    if let Some(tab) = session.active_tab_mut() {
                                        tab.layout.focus_pane(pane_id);
                                    }
                                    save_session(&session, &mut action_error);
                                }
                            }
                        }
                    }
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        if let Some(drag) = &split_drag {
                            let ratio = match drag.direction {
                                Direction::Horizontal => {
                                    (mouse.column.saturating_sub(drag.area.x)) as f32
                                        / drag.area.width.max(1) as f32
                                }
                                Direction::Vertical => {
                                    (mouse.row.saturating_sub(drag.area.y)) as f32
                                        / drag.area.height.max(1) as f32
                                }
                            };
                            if let Some(tab) = session.active_tab_mut() {
                                tab.layout.set_ratio_at(&drag.path, ratio);
                            }
                        }
                    }
                    MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        if split_drag.take().is_some() {
                            save_session(&session, &mut action_error);
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if let Some(pane_id) = hovered_pane(&layout, mouse.column, mouse.row) {
                            session.scroll_pane(pane_id, 3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if let Some(pane_id) = hovered_pane(&layout, mouse.column, mouse.row) {
                            session.scroll_pane(pane_id, -3);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
