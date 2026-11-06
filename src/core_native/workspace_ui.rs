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

pub(crate) fn run(discovery: CoreDiscovery, discovered: Vec<CoreTerminal>) -> io::Result<()> {
    let (context, mut events) = CoreRuntimeContext::new();
    let render_dirty = context.render_dirty();
    let snapshot = store::load()?;
    let discovered = discovery.enrich(discovered);
    let mut session =
        CoreSession::restore_or_create(snapshot, &discovery, context, discovered.clone())?;
    let (_core_listener, core_events) = CoreEventListener::spawn(&discovery)?;
    let mut terminal = init_terminal()?;
    let mut restore = TerminalRestore { active: true };
    let mut prefix = false;
    let mut move_pending = false;
    let mut action_error = if session.notices.is_empty() {
        None
    } else {
        Some(session.notices.join("\n"))
    };
    save_session(&session, &mut action_error);

    let size = terminal.size()?;
    let mut layout = view_layout(&session, Rect::new(0, 0, size.width, size.height));
    let mut needs_draw = true;
    loop {
        while let Ok(event) = events.try_recv() {
            session.handle_event(event);
            needs_draw = true;
        }
        while let Ok(event) = core_events.try_recv() {
            match event {
                CoreEvent::Disconnected(error) => {
                    action_error = Some(error);
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
                )
            })?;
            needs_draw = false;
        }

        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        needs_draw = true;
        match event::read()? {
            Event::Key(key) => {
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
                            let terminals =
                                discovery.discover().unwrap_or_else(|_| discovered.clone());
                            let mut candidates: Vec<CoreTerminal> = terminals
                                .into_iter()
                                .filter(|core| {
                                    core.state.as_deref() == Some("live")
                                        && !session.has_core_terminal(&core.address)
                                })
                                .collect();
                            candidates.sort_by(|a, b| a.address.cmp(&b.address));
                            restore.suspend();
                            let choice = super::ui::pick_adopt(&candidates);
                            terminal = init_terminal()?;
                            restore.active = true;
                            match choice {
                                Ok(Some(core)) => {
                                    if let Err(err) = session.adopt_terminal(&discovery, core) {
                                        action_error = Some(err.to_string());
                                    }
                                }
                                Ok(None) => {}
                                Err(err) => action_error = Some(err.to_string()),
                            }
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
            Event::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) =>
            {
                if let Some(info) = layout.panes.iter().find(|info| {
                    mouse.column >= info.rect.x
                        && mouse.column < info.rect.x.saturating_add(info.rect.width)
                        && mouse.row >= info.rect.y
                        && mouse.row < info.rect.y.saturating_add(info.rect.height)
                }) {
                    if let Some(tab) = session.active_tab_mut() {
                        tab.layout.focus_pane(info.id);
                    }
                }
            }
            _ => {}
        }
    }
}
