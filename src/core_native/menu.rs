//! Global menu, keybind help, and settings overlays for the core-native UI.
//!
//! The legacy client's settings editor depends on server snapshots; the
//! core-native path instead shows the effective local configuration and opens
//! the config file in $EDITOR, then reloads it.

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAction {
    Settings,
    Keybinds,
    ReloadConfig,
    Adopt,
    Detach,
    /// Only reachable from the settings overlay, not the menu list.
    EditConfig,
}

const MENU_ITEMS: &[(&str, MenuAction)] = &[
    ("settings", MenuAction::Settings),
    ("keybinds", MenuAction::Keybinds),
    ("reload config", MenuAction::ReloadConfig),
    ("adopt terminal", MenuAction::Adopt),
    ("detach", MenuAction::Detach),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overlay {
    Menu { highlighted: usize },
    Keybinds,
    Settings,
}

impl Overlay {
    pub(crate) fn menu() -> Self {
        Self::Menu { highlighted: 0 }
    }

    /// Handle a key; returns an action to run, `None` to keep the overlay
    /// open, and closes on esc/q where applicable.
    pub(crate) fn handle_key(&mut self, code: KeyCode) -> OverlayOutcome {
        match self {
            Self::Menu { highlighted } => match code {
                KeyCode::Esc | KeyCode::Char('q') => OverlayOutcome::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *highlighted = highlighted.saturating_sub(1);
                    OverlayOutcome::Keep
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *highlighted = (*highlighted + 1).min(MENU_ITEMS.len() - 1);
                    OverlayOutcome::Keep
                }
                KeyCode::Enter => OverlayOutcome::Run(MENU_ITEMS[*highlighted].1),
                _ => OverlayOutcome::Keep,
            },
            Self::Keybinds => match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => OverlayOutcome::Close,
                _ => OverlayOutcome::Keep,
            },
            Self::Settings => match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => OverlayOutcome::Close,
                KeyCode::Char('e') => OverlayOutcome::Run(MenuAction::EditConfig),
                _ => OverlayOutcome::Keep,
            },
        }
    }

    /// Mouse support: click a menu row to run it, click outside any overlay
    /// to dismiss, wheel moves the menu highlight.
    pub(crate) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        frame: Rect,
    ) -> Option<OverlayOutcome> {
        let area = overlay_area(self, frame);
        let inside = mouse.column >= area.x
            && mouse.column < area.x.saturating_add(area.width)
            && mouse.row >= area.y
            && mouse.row < area.y.saturating_add(area.height);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if !inside => Some(OverlayOutcome::Close),
            MouseEventKind::Down(MouseButton::Left) => match self {
                Self::Menu { highlighted } => {
                    let row = mouse.row.saturating_sub(area.y + 1);
                    let index = row as usize;
                    if index < MENU_ITEMS.len() {
                        *highlighted = index;
                        Some(OverlayOutcome::Run(MENU_ITEMS[index].1))
                    } else {
                        None
                    }
                }
                _ => None,
            },
            MouseEventKind::ScrollUp => match self {
                Self::Menu { highlighted } => {
                    *highlighted = highlighted.saturating_sub(1);
                    None
                }
                _ => None,
            },
            MouseEventKind::ScrollDown => match self {
                Self::Menu { highlighted } => {
                    *highlighted = (*highlighted + 1).min(MENU_ITEMS.len() - 1);
                    None
                }
                _ => None,
            },
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOutcome {
    Keep,
    Close,
    Run(MenuAction),
}

/// The clickable `[menu]` label in the header row.
pub(crate) fn menu_button_area(frame: Rect) -> Rect {
    let label = "[menu]";
    let width = label.len() as u16;
    Rect::new(
        frame.width.saturating_sub(width + 1),
        0,
        width.min(frame.width),
        1,
    )
}

pub(crate) fn menu_button_hit(frame: Rect, mouse: MouseEvent) -> bool {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return false;
    }
    let area = menu_button_area(frame);
    mouse.row == area.y
        && mouse.column >= area.x
        && mouse.column < area.x.saturating_add(area.width)
}

fn overlay_area(overlay: &Overlay, frame: Rect) -> Rect {
    let (width, height) = match overlay {
        Overlay::Menu { .. } => (24u16, MENU_ITEMS.len() as u16 + 2),
        Overlay::Keybinds => (64, 24),
        Overlay::Settings => (72, 20),
    };
    let width = width.min(frame.width);
    let height = height.min(frame.height);
    Rect::new(
        (frame.width - width) / 2,
        (frame.height - height) / 2,
        width,
        height,
    )
}

pub(crate) fn draw(frame: &mut ratatui::Frame, overlay: &Overlay, settings_lines: &[String]) {
    let area = overlay_area(overlay, frame.area());
    frame.render_widget(Clear, area);
    match overlay {
        Overlay::Menu { highlighted } => {
            let lines: Vec<Line> = MENU_ITEMS
                .iter()
                .enumerate()
                .map(|(index, (label, _))| {
                    if index == *highlighted {
                        Line::from(Span::styled(
                            format!(" ❯ {label}"),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else {
                        Line::from(format!("   {label}"))
                    }
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" menu ")),
                area,
            );
        }
        Overlay::Keybinds => {
            let lines: Vec<Line> = KEYBIND_HELP
                .iter()
                .map(|line| Line::from(line.to_string()))
                .collect();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(" keybinds "))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        Overlay::Settings => {
            let lines: Vec<Line> = settings_lines
                .iter()
                .map(|line| Line::from(line.clone()))
                .collect();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(" settings "))
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

const KEYBIND_HELP: &[&str] = &[
    " ctrl+b prefix",
    "",
    " v / -        split right / down",
    " x            close pane (kills a live terminal)",
    " D            drop a finished/missing pane",
    " a            adopt an unshown core terminal",
    " b            browse all core terminals",
    " z            zoom the focused pane",
    " c / N        new tab / new workspace",
    " n / p        next / previous tab",
    " ] / [        next / previous workspace",
    " r            refresh core state",
    " m            move pane (then a direction)",
    " H L K J      swap with a neighboring pane",
    " h j k l      focus a neighboring pane",
    " enter        reattach or resume the focused pane",
    " g            this menu",
    " q            save and detach the UI",
    "",
    " mouse: click spaces/panes, wheel-scroll a pane,",
    " click [menu] for this menu",
    "",
    " esc/q closes this help",
];

pub(crate) fn settings_lines(config: &crate::config::Config) -> Vec<String> {
    vec![
        format!(" config:   {}", crate::config::config_path().display()),
        format!(" backend:  {:?}", config.terminal.backend),
        format!(
            " core cli: {}",
            std::env::var("HERDR_CORE_CLI").unwrap_or_else(|_| "hc".into())
        ),
        format!(" store:    {}", super::store::store_path().display()),
        format!(
            " shell:    {}",
            if config.terminal.default_shell.is_empty() {
                "$SHELL (default)"
            } else {
                config.terminal.default_shell.as_str()
            }
        ),
        String::new(),
        " press e to edit the config in $EDITOR".into(),
        " (the TUI suspends; config reloads on return)".into(),
        String::new(),
        " esc/q closes settings".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{MenuAction, Overlay, OverlayOutcome, MENU_ITEMS};
    use crossterm::event::KeyCode;

    #[test]
    fn menu_navigation_clamps_and_runs_the_highlighted_action() {
        let mut overlay = Overlay::menu();
        assert_eq!(overlay.handle_key(KeyCode::Up), OverlayOutcome::Keep);
        assert_eq!(
            overlay.handle_key(KeyCode::Enter),
            OverlayOutcome::Run(MenuAction::Settings)
        );
        for _ in 0..20 {
            overlay.handle_key(KeyCode::Down);
        }
        assert_eq!(
            overlay.handle_key(KeyCode::Enter),
            OverlayOutcome::Run(*MENU_ITEMS.last().map(|(_, action)| action).expect("items"))
        );
        assert_eq!(overlay.handle_key(KeyCode::Esc), OverlayOutcome::Close);
    }

    #[test]
    fn help_and_settings_close_on_escape() {
        for mut overlay in [Overlay::Keybinds, Overlay::Settings] {
            assert_eq!(overlay.handle_key(KeyCode::Esc), OverlayOutcome::Close);
        }
    }
}
