use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::discovery::{CoreDiscovery, CoreTerminal};

struct TerminalRestore {
    active: bool,
}

impl TerminalRestore {
    fn suspend(&mut self) {
        if self.active {
            ratatui::restore();
            self.active = false;
        }
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        self.suspend();
    }
}

fn terminal_line(terminal: &CoreTerminal, selected: bool) -> Line<'static> {
    let name = terminal.name.as_deref().unwrap_or("terminal");
    let app = terminal
        .status_app()
        .or(terminal.agent.as_deref())
        .unwrap_or("shell");
    let status = terminal
        .status_state()
        .or(terminal.state.as_deref())
        .unwrap_or("unknown");
    let cwd = terminal.cwd.as_deref().unwrap_or("");
    let turn = terminal
        .turn
        .map(|turn| format!("t{turn} "))
        .unwrap_or_default();
    let marker = if selected { "❯ " } else { "  " };
    Line::from(vec![
        Span::styled(
            format!("{marker}{:<7}", terminal.address),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            format!("{name:<20}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{app:<14}")),
        Span::styled(format!("{status:<10}"), Style::default().fg(Color::Green)),
        Span::styled(turn, Style::default().fg(Color::DarkGray)),
        Span::styled(
            cwd.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
}

fn draw(
    frame: &mut ratatui::Frame,
    terminals: &[CoreTerminal],
    selected: usize,
    action_error: Option<&str>,
) {
    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "Herdr",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · core-native · direct core connection"),
    ]));
    frame.render_widget(header, areas[0]);

    let mut lines = Vec::new();
    if terminals.is_empty() {
        lines.push(Line::from(
            "No live core terminals found. Press n to create one.",
        ));
    } else {
        lines.extend(
            terminals
                .iter()
                .enumerate()
                .map(|(index, terminal)| terminal_line(terminal, index == selected)),
        );
    }
    if let Some(error) = action_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(Color::Red),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" core terminals "))
            .wrap(Wrap { trim: false }),
        areas[1],
    );
    frame.render_widget(
        Paragraph::new("↑↓/j/k select  ·  enter attach  ·  n new  ·  r refresh  ·  q quit")
            .style(Style::default().fg(Color::DarkGray)),
        areas[2],
    );
}

fn refresh(
    discovery: &CoreDiscovery,
    terminals: &mut Vec<CoreTerminal>,
    selected: &mut usize,
    action_error: &mut Option<String>,
) {
    let selected_address = terminals
        .get(*selected)
        .map(|terminal| terminal.address.clone());
    match discovery.discover() {
        Ok(discovered) => {
            *selected = selected_address
                .and_then(|address| {
                    discovered
                        .iter()
                        .position(|terminal| terminal.address == address)
                })
                .unwrap_or(0)
                .min(discovered.len().saturating_sub(1));
            *terminals = discovered;
            *action_error = None;
        }
        Err(err) => *action_error = Some(err.to_string()),
    }
}

fn attach(
    discovery: &CoreDiscovery,
    address: &str,
    terminal: &mut ratatui::DefaultTerminal,
    restore: &mut TerminalRestore,
) -> io::Result<()> {
    restore.suspend();
    let result = discovery.attach(address);
    *terminal = ratatui::init();
    restore.active = true;
    let status = result?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "core terminal {address} attach ended with {status}"
        )))
    }
}

pub(crate) fn run(discovery: CoreDiscovery, mut terminals: Vec<CoreTerminal>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut restore = TerminalRestore { active: true };
    let mut action_error = None;
    let mut selected = 0;

    loop {
        terminal.draw(|frame| draw(frame, &terminals, selected, action_error.as_deref()))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !terminals.is_empty() {
                        selected = (selected + 1).min(terminals.len() - 1);
                    }
                }
                KeyCode::Char('r') => {
                    refresh(&discovery, &mut terminals, &mut selected, &mut action_error)
                }
                KeyCode::Enter => {
                    if let Some(address) = terminals
                        .get(selected)
                        .map(|terminal| terminal.address.clone())
                    {
                        match attach(&discovery, &address, &mut terminal, &mut restore) {
                            Ok(()) => refresh(
                                &discovery,
                                &mut terminals,
                                &mut selected,
                                &mut action_error,
                            ),
                            Err(err) => action_error = Some(err.to_string()),
                        }
                    }
                }
                KeyCode::Char('n') => {
                    let cwd = std::env::current_dir()?;
                    match discovery.create_terminal(&cwd) {
                        Ok(created) => {
                            match attach(&discovery, &created.address, &mut terminal, &mut restore)
                            {
                                Ok(()) => {
                                    refresh(
                                        &discovery,
                                        &mut terminals,
                                        &mut selected,
                                        &mut action_error,
                                    );
                                    if let Some(index) = terminals
                                        .iter()
                                        .position(|terminal| terminal.address == created.address)
                                    {
                                        selected = index;
                                    }
                                }
                                Err(err) => action_error = Some(err.to_string()),
                            }
                        }
                        Err(err) => action_error = Some(err.to_string()),
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Pick a live core terminal to adopt into the workspace view. Returns
/// `Ok(None)` when the user cancels. Manages its own terminal session so the
/// caller's workspace UI stays suspended until the choice is made.
pub(crate) fn pick_adopt(terminals: &[CoreTerminal]) -> io::Result<Option<CoreTerminal>> {
    if terminals.is_empty() {
        return Ok(None);
    }
    let mut terminal = ratatui::init();
    let restore = TerminalRestore { active: true };
    let mut selected = 0usize;
    let choice = loop {
        terminal.draw(|frame| {
            let areas = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(frame.area());
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "Herdr",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" · core-native · adopt a core terminal"),
                ])),
                areas[0],
            );
            let lines: Vec<Line> = terminals
                .iter()
                .enumerate()
                .map(|(index, core)| terminal_line(core, index == selected))
                .collect();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(" unshown terminals "))
                    .wrap(Wrap { trim: false }),
                areas[1],
            );
            frame.render_widget(
                Paragraph::new("↑↓/j/k select · enter adopt · esc cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                areas[2],
            );
        })?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => break None,
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(terminals.len() - 1);
                }
                KeyCode::Enter => break Some(terminals[selected].clone()),
                _ => {}
            },
            _ => {}
        }
    };
    drop(restore);
    drop(terminal);
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::terminal_line;
    use crate::core_native::discovery::CoreTerminal;

    #[test]
    fn terminal_line_shows_core_identity_and_agent_status() {
        let terminal = CoreTerminal {
            address: "t12".into(),
            name: Some("worker".into()),
            status: Some(serde_json::json!({"app":"kimi-code","state":"done"})),
            cwd: Some("/repo".into()),
            ..CoreTerminal::default()
        };
        let text = terminal_line(&terminal, true)
            .spans
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(text.contains("❯"));
        assert!(text.contains("t12"));
        assert!(text.contains("worker"));
        assert!(text.contains("kimi-code"));
        assert!(text.contains("done"));
        assert!(text.contains("/repo"));
    }
}
