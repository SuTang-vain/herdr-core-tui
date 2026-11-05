use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::layout::{Direction, Rect};
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::{find_in_direction, PaneId, TileLayout};
use crate::pane::{AgentDetection, PaneLaunchEnv};
use crate::persist::{capture_node, restore_node_remapped};
use crate::render_signal::RenderSignal;
use crate::terminal::TerminalRuntime;

use super::discovery::{CoreDiscovery, CoreTerminal};
use super::events::{CoreChange, CoreEvent};
use super::resume;
use super::store::{
    CorePaneSnapshot, CoreSessionSnapshot, CoreTabSnapshot, CoreWorkspaceSnapshot,
    CORE_NATIVE_STORE_VERSION,
};

#[derive(Clone)]
pub(crate) struct CoreRuntimeContext {
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
}

impl CoreRuntimeContext {
    pub(crate) fn render_dirty(&self) -> Arc<RenderSignal> {
        self.render_dirty.clone()
    }

    pub(crate) fn new() -> (Self, mpsc::Receiver<AppEvent>) {
        let (events, receiver) = mpsc::channel(1024);
        (
            Self {
                events,
                render_notify: Arc::new(Notify::new()),
                render_dirty: Arc::new(RenderSignal::new()),
            },
            receiver,
        )
    }
}

pub(crate) struct CorePane {
    pub core: CoreTerminal,
    pub agent_session: Option<super::store::CoreAgentSessionSnapshot>,
    pub cwd: PathBuf,
    pub runtime: Option<TerminalRuntime>,
    pub last_size: (u16, u16),
}

impl CorePane {
    fn spawn_attach(
        pane_id: PaneId,
        address: &str,
        cwd: PathBuf,
        context: &CoreRuntimeContext,
    ) -> io::Result<TerminalRuntime> {
        let discovery = CoreDiscovery::from_env();
        TerminalRuntime::spawn_argv_command(
            pane_id,
            24,
            80,
            cwd,
            &discovery.attach_argv(address),
            &PaneLaunchEnv::default(),
            AgentDetection::Disabled,
            1_000_000,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            context.events.clone(),
            context.render_notify.clone(),
            context.render_dirty.clone(),
        )
    }

    fn create(
        pane_id: PaneId,
        cwd: PathBuf,
        discovery: &CoreDiscovery,
        context: &CoreRuntimeContext,
    ) -> io::Result<Self> {
        let core = discovery.create_terminal(&cwd)?;
        match Self::spawn_attach(pane_id, &core.address, cwd.clone(), context) {
            Ok(runtime) => Ok(Self {
                core,
                agent_session: None,
                cwd,
                runtime: Some(runtime),
                last_size: (24, 80),
            }),
            Err(err) => {
                let _ = discovery.kill_terminal(&core.address);
                Err(err)
            }
        }
    }

    fn restore(
        pane_id: PaneId,
        snapshot: CorePaneSnapshot,
        discovery: &CoreDiscovery,
        context: &CoreRuntimeContext,
    ) -> (Self, Option<String>) {
        let agent_session = snapshot
            .agent_session
            .or_else(|| resume::lookup(&snapshot.core_address));
        let mut notice = None;
        let (core, runtime) = match discovery.get_terminal(&snapshot.core_address) {
            Ok(core) if core.state.as_deref() == Some("live") => {
                match Self::spawn_attach(pane_id, &core.address, snapshot.cwd.clone(), context) {
                    Ok(runtime) => (core, Some(runtime)),
                    Err(err) => {
                        notice = Some(format!(
                            "{} is live but attach failed: {err}",
                            snapshot.core_address
                        ));
                        (core, None)
                    }
                }
            }
            Ok(core) => (core, None),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                notice = Some(format!(
                    "core terminal {} is missing",
                    snapshot.core_address
                ));
                (
                    CoreTerminal {
                        address: snapshot.core_address,
                        state: Some("missing".into()),
                        cwd: Some(snapshot.cwd.to_string_lossy().into_owned()),
                        ..CoreTerminal::default()
                    },
                    None,
                )
            }
            Err(err) => {
                notice = Some(format!(
                    "could not restore {}: {err}",
                    snapshot.core_address
                ));
                (
                    CoreTerminal {
                        address: snapshot.core_address,
                        cwd: Some(snapshot.cwd.to_string_lossy().into_owned()),
                        ..CoreTerminal::default()
                    },
                    None,
                )
            }
        };
        (
            Self {
                core,
                agent_session,
                cwd: snapshot.cwd,
                runtime,
                last_size: (24, 80),
            },
            notice,
        )
    }

    fn reattach(
        &mut self,
        pane_id: PaneId,
        discovery: &CoreDiscovery,
        context: &CoreRuntimeContext,
    ) -> io::Result<()> {
        self.core = discovery.get_terminal(&self.core.address)?;
        if self.core.state.as_deref() != Some("live") {
            return Err(io::Error::other(format!(
                "core terminal {} is not live",
                self.core.address
            )));
        }
        self.runtime = Some(Self::spawn_attach(
            pane_id,
            &self.core.address,
            self.cwd.clone(),
            context,
        )?);
        Ok(())
    }

    fn resume(
        &mut self,
        pane_id: PaneId,
        discovery: &CoreDiscovery,
        context: &CoreRuntimeContext,
    ) -> io::Result<()> {
        if self.agent_session.is_none() {
            self.agent_session = resume::lookup(&self.core.address);
        }
        let Some(session) = self.agent_session.clone() else {
            return Err(io::Error::other(
                "no recorded agent session; no new session was started",
            ));
        };
        let argv = resume::executable_argv(&session.resume_argv)?;
        let previous = self.core.address.clone();
        let core = discovery.create_agent_terminal(&self.cwd, &argv, &previous)?;
        match Self::spawn_attach(pane_id, &core.address, self.cwd.clone(), context) {
            Ok(runtime) => {
                self.core = core;
                self.runtime = Some(runtime);
                self.agent_session = Some(session);
                Ok(())
            }
            Err(err) => {
                let _ = discovery.kill_terminal(&core.address);
                Err(err)
            }
        }
    }

    fn refresh_agent_session(&mut self) {
        if self.core.status_app() == Some("kimi-code") {
            if let Some(session) = resume::lookup(&self.core.address) {
                self.agent_session = Some(session);
            }
        }
    }

    fn snapshot(&self) -> CorePaneSnapshot {
        CorePaneSnapshot {
            core_address: self.core.address.clone(),
            cwd: self.cwd.clone(),
            agent_session: self.agent_session.clone(),
        }
    }
}

pub(crate) struct CoreTab {
    pub id: String,
    pub name: Option<String>,
    pub layout: TileLayout,
    pub zoomed: bool,
    pub panes: HashMap<PaneId, CorePane>,
}

impl CoreTab {
    fn new(id: String) -> (Self, PaneId) {
        let (layout, root) = TileLayout::new();
        (
            Self {
                id,
                name: None,
                layout,
                zoomed: false,
                panes: HashMap::new(),
            },
            root,
        )
    }

    fn snapshot(&self) -> CoreTabSnapshot {
        CoreTabSnapshot {
            id: self.id.clone(),
            name: self.name.clone(),
            layout: capture_node(self.layout.root()),
            focused: self.layout.focused().raw(),
            zoomed: self.zoomed,
            panes: self
                .panes
                .iter()
                .map(|(id, pane)| (id.raw(), pane.snapshot()))
                .collect(),
        }
    }
}

pub(crate) struct CoreWorkspace {
    pub id: String,
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub active_tab: usize,
    pub tabs: Vec<CoreTab>,
}

impl CoreWorkspace {
    fn new(id: String, cwd: PathBuf) -> Self {
        Self {
            id,
            name: None,
            cwd,
            active_tab: 0,
            tabs: Vec::new(),
        }
    }

    pub fn active_tab(&self) -> Option<&CoreTab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut CoreTab> {
        self.tabs.get_mut(self.active_tab)
    }

    fn snapshot(&self) -> CoreWorkspaceSnapshot {
        CoreWorkspaceSnapshot {
            id: self.id.clone(),
            name: self.name.clone(),
            cwd: self.cwd.clone(),
            active_tab: self.active_tab,
            tabs: self.tabs.iter().map(CoreTab::snapshot).collect(),
        }
    }
}

#[derive(Default)]
pub(crate) struct CloseOutcome {
    pub closed: bool,
    pub exit_client: bool,
}

pub(crate) struct CoreSession {
    pub workspaces: Vec<CoreWorkspace>,
    pub active_workspace: usize,
    pub notices: Vec<String>,
    context: CoreRuntimeContext,
}

impl CoreSession {
    pub fn active_workspace(&self) -> Option<&CoreWorkspace> {
        self.workspaces.get(self.active_workspace)
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut CoreWorkspace> {
        self.workspaces.get_mut(self.active_workspace)
    }

    pub fn active_tab(&self) -> Option<&CoreTab> {
        self.active_workspace()?.active_tab()
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut CoreTab> {
        self.active_workspace_mut()?.active_tab_mut()
    }

    pub fn focused_pane(&self) -> Option<&CorePane> {
        let tab = self.active_tab()?;
        tab.panes.get(&tab.layout.focused())
    }

    pub fn focused_pane_mut(&mut self) -> Option<&mut CorePane> {
        let tab = self.active_tab_mut()?;
        tab.panes.get_mut(&tab.layout.focused())
    }

    pub fn restore_or_create(
        snapshot: CoreSessionSnapshot,
        discovery: &CoreDiscovery,
        context: CoreRuntimeContext,
    ) -> io::Result<Self> {
        if snapshot.version > CORE_NATIVE_STORE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported core-native store version {}", snapshot.version),
            ));
        }
        let mut session = Self {
            workspaces: Vec::new(),
            active_workspace: snapshot.active_workspace,
            notices: Vec::new(),
            context,
        };
        for workspace in snapshot.workspaces {
            let mut restored_workspace = CoreWorkspace::new(workspace.id, workspace.cwd);
            restored_workspace.name = workspace.name;
            restored_workspace.active_tab = workspace.active_tab;
            for tab in workspace.tabs {
                let (root, id_map) = restore_node_remapped(&tab.layout);
                let focused = id_map
                    .get(&tab.focused)
                    .copied()
                    .or_else(|| root_pane_id(&root))
                    .ok_or_else(|| io::Error::other("native tab has no root pane"))?;
                let mut restored_tab = CoreTab {
                    id: tab.id,
                    name: tab.name,
                    layout: TileLayout::from_saved(root, focused),
                    zoomed: tab.zoomed,
                    panes: HashMap::new(),
                };
                for (old_id, pane) in tab.panes {
                    let Some(pane_id) = id_map.get(&old_id).copied() else {
                        continue;
                    };
                    let (pane, notice) =
                        CorePane::restore(pane_id, pane, discovery, &session.context);
                    restored_tab.panes.insert(pane_id, pane);
                    session.notices.extend(notice);
                }
                restored_workspace.tabs.push(restored_tab);
            }
            session.workspaces.push(restored_workspace);
        }
        if session.workspaces.is_empty() {
            session.add_workspace(discovery, std::env::current_dir()?)?;
        }
        session.active_workspace = session
            .active_workspace
            .min(session.workspaces.len().saturating_sub(1));
        Ok(session)
    }

    pub fn add_workspace(&mut self, discovery: &CoreDiscovery, cwd: PathBuf) -> io::Result<()> {
        let id = format!("w{}", PaneId::alloc().raw());
        let mut workspace = CoreWorkspace::new(id, cwd.clone());
        let (tab, root) = CoreTab::new(format!("t{}", PaneId::alloc().raw()));
        workspace.tabs.push(tab);
        let pane = CorePane::create(root, cwd, discovery, &self.context)?;
        workspace.tabs[0].panes.insert(root, pane);
        self.workspaces.push(workspace);
        self.active_workspace = self.workspaces.len() - 1;
        Ok(())
    }

    pub fn add_tab(&mut self, discovery: &CoreDiscovery) -> io::Result<()> {
        let cwd = self
            .active_workspace()
            .map(|workspace| workspace.cwd.clone())
            .unwrap_or_else(|| PathBuf::from("/"));
        let (tab, root) = CoreTab::new(format!("t{}", PaneId::alloc().raw()));
        let pane = CorePane::create(root, cwd, discovery, &self.context)?;
        let workspace = self
            .active_workspace_mut()
            .ok_or_else(|| io::Error::other("no active workspace"))?;
        workspace.tabs.push(tab);
        let index = workspace.tabs.len() - 1;
        workspace.tabs[index].panes.insert(root, pane);
        workspace.active_tab = index;
        Ok(())
    }

    pub fn split(&mut self, discovery: &CoreDiscovery, direction: Direction) -> io::Result<()> {
        let cwd = self
            .focused_pane()
            .map(|pane| pane.cwd.clone())
            .or_else(|| {
                self.active_workspace()
                    .map(|workspace| workspace.cwd.clone())
            })
            .unwrap_or_else(|| PathBuf::from("/"));
        let context = self.context.clone();
        let tab = self
            .active_tab_mut()
            .ok_or_else(|| io::Error::other("no active tab"))?;
        let target = tab.layout.focused();
        let pane_id = tab
            .layout
            .split_pane(target, direction, 0.5)
            .ok_or_else(|| io::Error::other("focused pane is not in the layout"))?;
        match CorePane::create(pane_id, cwd, discovery, &context) {
            Ok(pane) => {
                tab.panes.insert(pane_id, pane);
                tab.layout.focus_pane(pane_id);
                tab.zoomed = false;
                Ok(())
            }
            Err(err) => {
                tab.layout.close_pane(pane_id);
                Err(err)
            }
        }
    }

    pub fn reattach_focused(&mut self, discovery: &CoreDiscovery) -> io::Result<()> {
        let context = self.context.clone();
        let tab = self
            .active_tab_mut()
            .ok_or_else(|| io::Error::other("no active tab"))?;
        let pane_id = tab.layout.focused();
        let pane = tab
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| io::Error::other("focused pane is missing"))?;
        if pane.runtime.is_some() {
            return Ok(());
        }
        pane.reattach(pane_id, discovery, &context)
    }

    pub fn restore_focused(&mut self, discovery: &CoreDiscovery) -> io::Result<()> {
        let state = self
            .focused_pane()
            .and_then(|pane| pane.core.state.as_deref())
            .unwrap_or("missing");
        match state {
            "live" => self.reattach_focused(discovery),
            "finished" => {
                let context = self.context.clone();
                let tab = self
                    .active_tab_mut()
                    .ok_or_else(|| io::Error::other("no active tab"))?;
                let pane_id = tab.layout.focused();
                let pane = tab
                    .panes
                    .get_mut(&pane_id)
                    .ok_or_else(|| io::Error::other("focused pane is missing"))?;
                pane.resume(pane_id, discovery, &context)
            }
            _ => Err(io::Error::other(
                "core terminal is missing; no replacement was started",
            )),
        }
    }

    pub fn close_focused(&mut self, discovery: &CoreDiscovery) -> io::Result<CloseOutcome> {
        let tab = self
            .active_tab_mut()
            .ok_or_else(|| io::Error::other("no active tab"))?;
        let pane_id = tab.layout.focused();
        let pane = tab
            .panes
            .get(&pane_id)
            .ok_or_else(|| io::Error::other("focused pane is missing"))?;
        if pane.core.state.as_deref() == Some("live") {
            discovery.kill_terminal(&pane.core.address)?;
        }
        tab.panes.remove(&pane_id);
        if tab.layout.pane_count() > 1 {
            tab.layout.close_pane(pane_id);
            return Ok(CloseOutcome {
                closed: true,
                exit_client: false,
            });
        }

        let workspace = self
            .active_workspace_mut()
            .ok_or_else(|| io::Error::other("no active workspace"))?;
        workspace.tabs.remove(workspace.active_tab);
        if !workspace.tabs.is_empty() {
            workspace.active_tab = workspace
                .active_tab
                .min(workspace.tabs.len().saturating_sub(1));
            return Ok(CloseOutcome {
                closed: true,
                exit_client: false,
            });
        }
        self.workspaces.remove(self.active_workspace);
        self.active_workspace = self
            .active_workspace
            .min(self.workspaces.len().saturating_sub(1));
        Ok(CloseOutcome {
            closed: true,
            exit_client: self.workspaces.is_empty(),
        })
    }

    pub fn toggle_zoom(&mut self) {
        if let Some(tab) = self.active_tab_mut() {
            tab.zoomed = !tab.zoomed;
        }
    }

    pub fn next_tab(&mut self, delta: isize) {
        if let Some(workspace) = self.active_workspace_mut() {
            if !workspace.tabs.is_empty() {
                workspace.active_tab =
                    wrap_index(workspace.active_tab, workspace.tabs.len(), delta);
            }
        }
    }

    pub fn next_workspace(&mut self, delta: isize) {
        if !self.workspaces.is_empty() {
            self.active_workspace = wrap_index(self.active_workspace, self.workspaces.len(), delta);
        }
    }

    pub fn focus_direction(&mut self, direction: crate::layout::NavDirection, area: Rect) {
        if let Some(tab) = self.active_tab_mut() {
            let panes = tab.layout.panes(area);
            let focused = tab.layout.focused();
            if let Some(target) = panes
                .iter()
                .find(|info| info.id == focused)
                .and_then(|info| find_in_direction(info, direction, &panes))
            {
                tab.layout.focus_pane(target);
            }
        }
    }

    pub fn swap_direction(&mut self, direction: crate::layout::NavDirection, area: Rect) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let panes = tab.layout.panes(area);
        let focused = tab.layout.focused();
        panes
            .iter()
            .find(|info| info.id == focused)
            .and_then(|info| find_in_direction(info, direction, &panes))
            .is_some_and(|target| tab.layout.swap_panes(focused, target))
    }

    pub fn move_direction(&mut self, direction: crate::layout::NavDirection, area: Rect) -> bool {
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let panes = tab.layout.panes(area);
        let focused = tab.layout.focused();
        panes
            .iter()
            .find(|info| info.id == focused)
            .and_then(|info| find_in_direction(info, direction, &panes))
            .is_some_and(|target| {
                let layout_direction = match direction {
                    crate::layout::NavDirection::Left | crate::layout::NavDirection::Right => {
                        Direction::Horizontal
                    }
                    crate::layout::NavDirection::Up | crate::layout::NavDirection::Down => {
                        Direction::Vertical
                    }
                };
                tab.layout
                    .move_pane(focused, target, layout_direction, 0.5, true)
            })
    }

    pub fn resize_pane(&mut self, pane_id: PaneId, rows: u16, cols: u16) {
        if let Some(pane) = self.find_pane_mut(pane_id) {
            if pane.last_size != (rows, cols) {
                if let Some(runtime) = &pane.runtime {
                    runtime.resize(rows, cols, 0, 0);
                }
                pane.last_size = (rows, cols);
            }
        }
    }

    pub fn send_key(&mut self, key: crossterm::event::KeyEvent) -> io::Result<()> {
        let pane = self
            .focused_pane_mut()
            .ok_or_else(|| io::Error::other("no focused pane"))?;
        let runtime = pane
            .runtime
            .as_ref()
            .ok_or_else(|| io::Error::other("focused core terminal is detached"))?;
        let terminal_key: crate::input::TerminalKey = key.into();
        let bytes = runtime.encode_terminal_key(terminal_key);
        runtime
            .try_send_bytes(bytes::Bytes::from(bytes))
            .map_err(|err| io::Error::other(err.to_string()))
    }

    pub fn send_paste(&mut self, text: String) -> io::Result<()> {
        let pane = self
            .focused_pane_mut()
            .ok_or_else(|| io::Error::other("no focused pane"))?;
        let runtime = pane
            .runtime
            .as_ref()
            .ok_or_else(|| io::Error::other("focused core terminal is detached"))?;
        runtime
            .try_send_paste(text)
            .map_err(|err| io::Error::other(err.to_string()))
    }

    pub fn handle_event(&mut self, event: AppEvent) -> bool {
        let AppEvent::PaneDied { pane_id, .. } = event else {
            return false;
        };
        if let Some(pane) = self.find_pane_mut(pane_id) {
            pane.runtime = None;
            true
        } else {
            false
        }
    }

    pub fn apply_core_event(&mut self, event: CoreEvent) -> bool {
        match event {
            CoreEvent::Snapshot(terminals) => {
                let mut changed = false;
                for terminal in terminals {
                    changed |= self.apply_core_terminal(terminal);
                }
                changed
            }
            CoreEvent::Changed(change) => self.apply_core_change(change),
            CoreEvent::Disconnected(_) => false,
        }
    }

    fn apply_core_terminal(&mut self, core: CoreTerminal) -> bool {
        if let Some(pane) = self.find_pane_by_address(&core.address) {
            pane.core = core;
            pane.refresh_agent_session();
            true
        } else {
            false
        }
    }

    fn apply_core_change(&mut self, change: CoreChange) -> bool {
        let Some(pane) = self.find_pane_by_address(&change.address) else {
            return false;
        };
        match change.kind.as_str() {
            "created" | "restarted" => match serde_json::from_value::<CoreTerminal>(change.data) {
                Ok(core) => pane.core = core,
                Err(_) => pane.core.state = Some("live".into()),
            },
            "status" => {
                let status = change.data.get("status").cloned().unwrap_or(change.data);
                pane.core.status = (!status.is_null()).then_some(status);
            }
            "finished" => {
                pane.core.state = Some("finished".into());
                pane.core.status = None;
            }
            "removed" => {
                pane.core.state = Some("missing".into());
                pane.core.status = None;
                pane.runtime = None;
            }
            "turn" => {
                pane.core.turn = change
                    .data
                    .as_u64()
                    .or_else(|| change.data.get("turn")?.as_u64());
            }
            _ => return false,
        }
        pane.refresh_agent_session();
        true
    }

    pub fn refresh_core(&mut self, discovery: &CoreDiscovery) {
        for workspace in &mut self.workspaces {
            for tab in &mut workspace.tabs {
                for pane in tab.panes.values_mut() {
                    match discovery.get_terminal(&pane.core.address) {
                        Ok(core) => {
                            pane.core = core;
                            pane.refresh_agent_session();
                        }
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {
                            pane.core.state = Some("missing".into());
                            pane.runtime = None;
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }

    fn find_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut CorePane> {
        self.workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find_map(|tab| tab.panes.get_mut(&pane_id))
    }

    fn find_pane_by_address(&mut self, address: &str) -> Option<&mut CorePane> {
        self.workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .flat_map(|tab| tab.panes.values_mut())
            .find(|pane| pane.core.address == address)
    }

    pub fn snapshot(&self) -> CoreSessionSnapshot {
        CoreSessionSnapshot {
            version: CORE_NATIVE_STORE_VERSION,
            active_workspace: self.active_workspace,
            workspaces: self
                .workspaces
                .iter()
                .map(CoreWorkspace::snapshot)
                .collect(),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        super::store::save(&self.snapshot())
    }
}

fn root_pane_id(node: &crate::layout::Node) -> Option<PaneId> {
    match node {
        crate::layout::Node::Pane(id) => Some(*id),
        crate::layout::Node::Split { first, .. } => root_pane_id(first),
    }
}

fn wrap_index(current: usize, len: usize, delta: isize) -> usize {
    let len = len as isize;
    let current = current as isize;
    (current + delta).rem_euclid(len) as usize
}

#[cfg(test)]
mod tests {
    use super::{CorePane, CoreSession, CoreTab, CoreWorkspace};
    use crate::core_native::discovery::CoreTerminal;
    use crate::core_native::events::{CoreChange, CoreEvent};
    use std::path::PathBuf;

    fn session_with_pane() -> CoreSession {
        let (context, _events) = super::CoreRuntimeContext::new();
        let (tab, root) = CoreTab::new("tab1".into());
        let mut tab = tab;
        tab.panes.insert(
            root,
            CorePane {
                core: CoreTerminal {
                    address: "t1".into(),
                    state: Some("live".into()),
                    ..CoreTerminal::default()
                },
                agent_session: None,
                cwd: PathBuf::from("/repo"),
                runtime: None,
                last_size: (24, 80),
            },
        );
        let mut workspace = CoreWorkspace::new("w1".into(), PathBuf::from("/repo"));
        workspace.tabs.push(tab);
        CoreSession {
            workspaces: vec![workspace],
            active_workspace: 0,
            notices: Vec::new(),
            context,
        }
    }

    #[test]
    fn workspace_and_tab_navigation_wraps() {
        assert_eq!(super::wrap_index(0, 3, -1), 2);
        assert_eq!(super::wrap_index(2, 3, 1), 0);
        assert_eq!(super::wrap_index(1, 3, 1), 2);
    }

    #[test]
    fn core_events_update_only_the_matching_pane() {
        let mut session = session_with_pane();
        assert!(session.apply_core_event(CoreEvent::Changed(CoreChange {
            kind: "status".into(),
            address: "t1".into(),
            data: serde_json::json!({"app":"kimi-code","state":"working"}),
        })));
        assert_eq!(
            session
                .focused_pane()
                .and_then(|pane| pane.core.status_state()),
            Some("working")
        );
        assert!(!session.apply_core_event(CoreEvent::Changed(CoreChange {
            kind: "status".into(),
            address: "other".into(),
            data: serde_json::json!({"state":"done"}),
        })));
        session.apply_core_event(CoreEvent::Changed(CoreChange {
            kind: "finished".into(),
            address: "t1".into(),
            data: serde_json::json!({"reason":"exited"}),
        }));
        assert_eq!(
            session
                .focused_pane()
                .and_then(|pane| pane.core.state.as_deref()),
            Some("finished")
        );
        assert_eq!(
            session
                .focused_pane()
                .and_then(|pane| pane.core.status_state()),
            None
        );
    }
}
