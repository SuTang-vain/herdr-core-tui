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

use super::discovery::{CoreDiscovery, CoreTerminal, SPACE_LABEL, TAB_LABEL, WHERE_LABEL};
use super::events::{CoreChange, CoreEvent};
use super::resume;
use super::store::{
    CorePaneSnapshot, CoreSessionSnapshot, CoreTabSnapshot, CoreWorkspaceSnapshot,
    CORE_NATIVE_STORE_VERSION,
};

/// Group live core terminals into workspace/tab skeletons using the
/// `herdr.space`/`herdr.tab`/`where` labels shared with other core clients.
/// Terminals without labels are collected separately for later adoption.
#[derive(Default)]
struct CoreDerivedLayout {
    workspaces: Vec<DerivedWorkspace>,
    unorganized: Vec<CoreTerminal>,
}

struct DerivedWorkspace {
    id: String,
    name: Option<String>,
    tabs: Vec<DerivedTab>,
}

struct DerivedTab {
    id: String,
    name: Option<String>,
    terminals: Vec<CoreTerminal>,
}

impl CoreDerivedLayout {
    fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    fn derive(terminals: Vec<CoreTerminal>) -> Self {
        let mut derived = Self::default();
        for core in terminals {
            if core.state.as_deref() != Some("live") {
                continue;
            }
            let Some(space) = core.space_label() else {
                derived.unorganized.push(core);
                continue;
            };
            let Some(tab_label) = core.tab_label() else {
                derived.unorganized.push(core);
                continue;
            };
            let (where_space, where_tab) = core
                .where_parts()
                .map(|(w, t)| (Some(w), Some(t)))
                .unwrap_or((None, None));
            let workspace = match derived
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == space)
            {
                Some(workspace) => workspace,
                None => {
                    derived.workspaces.push(DerivedWorkspace {
                        id: space.to_string(),
                        name: where_space,
                        tabs: Vec::new(),
                    });
                    derived.workspaces.last_mut().expect("just pushed")
                }
            };
            let tab = match workspace.tabs.iter_mut().find(|tab| tab.id == tab_label) {
                Some(tab) => tab,
                None => {
                    workspace.tabs.push(DerivedTab {
                        id: tab_label.to_string(),
                        name: where_tab,
                        terminals: Vec::new(),
                    });
                    workspace.tabs.last_mut().expect("just pushed")
                }
            };
            tab.terminals.push(core);
        }
        derived
    }

    /// Build a minimal flat layout for one tab's terminals, splitting to the
    /// right of the previous pane. Returns None when the tab has no panes.
    fn build_tab(id: String, name: Option<String>, count: usize) -> Option<(CoreTab, Vec<PaneId>)> {
        if count == 0 {
            return None;
        }
        let (mut tab, root) = CoreTab::new(id);
        tab.name = name;
        let mut ids = vec![root];
        let mut previous = root;
        for _ in 1..count {
            let Some(id) = tab.layout.split_pane(previous, Direction::Horizontal, 0.5) else {
                break;
            };
            ids.push(id);
            previous = id;
        }
        Some((tab, ids))
    }
}

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

    fn adopt(
        pane_id: PaneId,
        core: CoreTerminal,
        discovery: &CoreDiscovery,
        context: &CoreRuntimeContext,
    ) -> (Self, Option<String>) {
        let cwd = core
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let snapshot = CorePaneSnapshot {
            core_address: core.address.clone(),
            cwd,
            agent_session: None,
        };
        let (mut pane, notice) = Self::restore(pane_id, snapshot, discovery, context);
        // Adopted terminals keep their organization; record it for the store.
        pane.core = core;
        (pane, notice)
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

/// Organization shared with other core clients through terminal labels.
#[derive(Default)]
struct CoreOrganization {
    labels: HashMap<String, Vec<String>>,
}

impl CoreOrganization {
    fn record(&mut self, address: &str, core: &CoreTerminal) {
        let mut labels = Vec::new();
        if let Some(space) = core.space_label() {
            labels.push(format!("{SPACE_LABEL}={space}"));
        }
        if let Some(tab) = core.tab_label() {
            labels.push(format!("{TAB_LABEL}={tab}"));
        }
        if let Some(value) = core.labels.get(WHERE_LABEL) {
            labels.push(format!("{WHERE_LABEL}={value}"));
        }
        if !labels.is_empty() {
            self.labels.insert(address.to_string(), labels);
        }
    }

    /// Fill in labels from the workspace/tab that owns the pane. Keeps an
    /// already-recorded `where` name when it exists.
    fn for_new_pane(&self, workspace: &CoreWorkspace, tab: &CoreTab) -> Vec<String> {
        vec![
            format!("{SPACE_LABEL}={}", workspace.id),
            format!("{TAB_LABEL}={}", tab.id),
            format!(
                "{WHERE_LABEL}={} / {}",
                workspace.name.as_deref().unwrap_or(&workspace.id),
                tab.name.as_deref().unwrap_or(&tab.id)
            ),
        ]
    }
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
        live_terminals: Vec<CoreTerminal>,
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
        let mut derived = CoreDerivedLayout::derive(live_terminals);
        if session.workspaces.is_empty() && !derived.is_empty() {
            // The store is empty (fresh start or never organized): rebuild the
            // whole view from the labels other core clients already wrote.
            session.build_derived(&derived, discovery)?;
        } else {
            session.merge_derived(&mut derived, discovery);
        }
        if session.workspaces.is_empty() {
            session.add_workspace(discovery, std::env::current_dir()?)?;
        }
        if !derived.unorganized.is_empty() {
            session.notices.push(format!(
                "{} core terminal(s) have no workspace labels; adopt them with ctrl+b a",
                derived.unorganized.len()
            ));
        }
        session.active_workspace = session
            .active_workspace
            .min(session.workspaces.len().saturating_sub(1));
        Ok(session)
    }

    fn build_derived(
        &mut self,
        derived: &CoreDerivedLayout,
        discovery: &CoreDiscovery,
    ) -> io::Result<()> {
        for workspace in &derived.workspaces {
            let cwd = workspace
                .tabs
                .iter()
                .flat_map(|tab| &tab.terminals)
                .find_map(|core| core.cwd.as_deref().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let mut restored = CoreWorkspace::new(workspace.id.clone(), cwd);
            restored.name = workspace.name.clone();
            for tab in &workspace.tabs {
                let Some((mut new_tab, ids)) = CoreDerivedLayout::build_tab(
                    tab.id.clone(),
                    tab.name.clone(),
                    tab.terminals.len(),
                ) else {
                    continue;
                };
                for (pane_id, core) in ids.into_iter().zip(tab.terminals.iter().cloned()) {
                    let (pane, notice) = CorePane::adopt(pane_id, core, discovery, &self.context);
                    new_tab.panes.insert(pane_id, pane);
                    self.notices.extend(notice);
                }
                restored.tabs.push(new_tab);
            }
            if !restored.tabs.is_empty() {
                self.workspaces.push(restored);
            }
        }
        Ok(())
    }

    /// Add core terminals that appeared after the local snapshot was saved.
    /// Existing panes keep their positions; only unknown addresses are added.
    fn merge_derived(&mut self, derived: &mut CoreDerivedLayout, discovery: &CoreDiscovery) {
        let mut organization = CoreOrganization::default();
        for workspace in derived.workspaces.drain(..) {
            let cwd = workspace
                .tabs
                .iter()
                .flat_map(|tab| &tab.terminals)
                .find_map(|core| core.cwd.as_deref().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let workspace_index = match self
                .workspaces
                .iter_mut()
                .enumerate()
                .find(|(_, existing)| existing.id == workspace.id)
            {
                Some((index, existing)) => {
                    if existing.name.is_none() {
                        existing.name = workspace.name.clone();
                    }
                    index
                }
                None => {
                    let mut created = CoreWorkspace::new(workspace.id.clone(), cwd);
                    created.name = workspace.name.clone();
                    self.workspaces.push(created);
                    self.workspaces.len() - 1
                }
            };
            for tab in workspace.tabs {
                let new_terminals: Vec<CoreTerminal> = tab
                    .terminals
                    .into_iter()
                    .filter(|core| {
                        if self.find_pane_by_address(&core.address).is_some() {
                            return false;
                        }
                        organization.record(&core.address, core);
                        true
                    })
                    .collect();
                if new_terminals.is_empty() {
                    continue;
                }
                let target = self.workspaces[workspace_index]
                    .tabs
                    .iter_mut()
                    .find(|existing| existing.id == tab.id);
                match target {
                    Some(existing) => {
                        if existing.name.is_none() {
                            existing.name = tab.name.clone();
                        }
                        let mut previous = existing.layout.focused();
                        for core in new_terminals {
                            let Some(pane_id) =
                                existing
                                    .layout
                                    .split_pane(previous, Direction::Horizontal, 0.5)
                            else {
                                continue;
                            };
                            let (pane, notice) =
                                CorePane::adopt(pane_id, core, discovery, &self.context);
                            existing.panes.insert(pane_id, pane);
                            self.notices.extend(notice);
                            previous = pane_id;
                        }
                    }
                    None => {
                        let Some((mut new_tab, ids)) = CoreDerivedLayout::build_tab(
                            tab.id.clone(),
                            tab.name.clone(),
                            new_terminals.len(),
                        ) else {
                            continue;
                        };
                        for (pane_id, core) in ids.into_iter().zip(new_terminals) {
                            let (pane, notice) =
                                CorePane::adopt(pane_id, core, discovery, &self.context);
                            new_tab.panes.insert(pane_id, pane);
                            self.notices.extend(notice);
                        }
                        self.workspaces[workspace_index].tabs.push(new_tab);
                    }
                }
            }
        }
        for (address, labels) in organization.labels.drain() {
            self.label_created(discovery, &address, labels);
        }
    }

    pub fn add_workspace(&mut self, discovery: &CoreDiscovery, cwd: PathBuf) -> io::Result<()> {
        let id = format!("w{}", PaneId::alloc().raw());
        let mut workspace = CoreWorkspace::new(id, cwd.clone());
        let (tab, root) = CoreTab::new(format!("t{}", PaneId::alloc().raw()));
        let pane = CorePane::create(root, cwd, discovery, &self.context)?;
        let labels = CoreOrganization::default().for_new_pane(&workspace, &tab);
        workspace.tabs.push(tab);
        workspace.tabs[0].panes.insert(root, pane);
        let address = workspace.tabs[0].panes[&root].core.address.clone();
        self.label_created(discovery, &address, labels);
        self.workspaces.push(workspace);
        self.active_workspace = self.workspaces.len() - 1;
        Ok(())
    }

    /// Labels a terminal created by this client; a labeling failure leaves the
    /// terminal usable but unorganized, so it is surfaced as a notice only.
    fn label_created(&mut self, discovery: &CoreDiscovery, address: &str, labels: Vec<String>) {
        if let Err(err) = discovery.set_labels(address, &labels) {
            self.notices.push(format!(
                "created {address} without organization labels: {err}"
            ));
        }
    }

    pub fn add_tab(&mut self, discovery: &CoreDiscovery) -> io::Result<()> {
        let cwd = self
            .active_workspace()
            .map(|workspace| workspace.cwd.clone())
            .unwrap_or_else(|| PathBuf::from("/"));
        let (tab, root) = CoreTab::new(format!("t{}", PaneId::alloc().raw()));
        let pane = CorePane::create(root, cwd, discovery, &self.context)?;
        let address = pane.core.address.clone();
        let workspace = self
            .active_workspace_mut()
            .ok_or_else(|| io::Error::other("no active workspace"))?;
        workspace.tabs.push(tab);
        let index = workspace.tabs.len() - 1;
        workspace.tabs[index].panes.insert(root, pane);
        workspace.active_tab = index;
        let labels = CoreOrganization::default().for_new_pane(workspace, &workspace.tabs[index]);
        self.label_created(discovery, &address, labels);
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
                let address = pane.core.address.clone();
                tab.panes.insert(pane_id, pane);
                tab.layout.focus_pane(pane_id);
                tab.zoomed = false;
                let labels = self
                    .active_workspace()
                    .zip(self.active_tab())
                    .map(|(workspace, tab)| {
                        CoreOrganization::default().for_new_pane(workspace, tab)
                    })
                    .unwrap_or_default();
                self.label_created(discovery, &address, labels);
                Ok(())
            }
            Err(err) => {
                tab.layout.close_pane(pane_id);
                Err(err)
            }
        }
    }

    /// Adopt an existing core terminal into the focused tab without replacing
    /// any pane. The terminal keeps running on core; only the view changes.
    pub fn adopt_terminal(
        &mut self,
        discovery: &CoreDiscovery,
        core: CoreTerminal,
    ) -> io::Result<()> {
        if self.find_pane_by_address(&core.address).is_some() {
            return Err(io::Error::other(format!(
                "{} is already shown in this session",
                core.address
            )));
        }
        if core.state.as_deref() != Some("live") {
            return Err(io::Error::other(format!(
                "{} is not live; only live terminals can be adopted",
                core.address
            )));
        }
        let context = self.context.clone();
        let tab = self
            .active_tab_mut()
            .ok_or_else(|| io::Error::other("no active tab"))?;
        let target = tab.layout.focused();
        let pane_id = tab
            .layout
            .split_pane(target, ratatui::layout::Direction::Horizontal, 0.5)
            .ok_or_else(|| io::Error::other("focused pane is not in the layout"))?;
        let (pane, notice) = CorePane::adopt(pane_id, core, discovery, &context);
        tab.panes.insert(pane_id, pane);
        tab.layout.focus_pane(pane_id);
        tab.zoomed = false;
        self.notices.extend(notice);
        Ok(())
    }

    /// Remove a pane whose core terminal is gone, without killing anything.
    pub fn remove_focused_missing(&mut self) -> io::Result<CloseOutcome> {
        let state = self
            .focused_pane()
            .and_then(|pane| pane.core.state.as_deref())
            .unwrap_or("missing");
        if state == "live" {
            return Err(io::Error::other(
                "focused pane is live; use close to kill its terminal",
            ));
        }
        self.close_focused_inner(None)
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
        self.close_focused_inner(Some(discovery))
    }

    /// Close the focused pane. With a discovery, a live core terminal is
    /// killed; without one, the pane is only removed from the view.
    fn close_focused_inner(
        &mut self,
        discovery: Option<&CoreDiscovery>,
    ) -> io::Result<CloseOutcome> {
        let tab = self
            .active_tab_mut()
            .ok_or_else(|| io::Error::other("no active tab"))?;
        let pane_id = tab.layout.focused();
        let pane = tab
            .panes
            .get(&pane_id)
            .ok_or_else(|| io::Error::other("focused pane is missing"))?;
        if pane.core.state.as_deref() == Some("live") {
            if let Some(discovery) = discovery {
                discovery.kill_terminal(&pane.core.address)?;
            }
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

    /// Scroll a pane's viewport without changing focus (mouse wheel).
    pub fn scroll_pane(&mut self, pane_id: PaneId, delta: isize) {
        if let Some(pane) = self.find_pane_mut(pane_id) {
            if let Some(runtime) = &pane.runtime {
                if delta > 0 {
                    runtime.scroll_up(delta as usize);
                } else if delta < 0 {
                    runtime.scroll_down(delta.unsigned_abs());
                }
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
            CoreEvent::Heartbeat | CoreEvent::ResyncNeeded => false,
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

    pub fn has_core_terminal(&self, address: &str) -> bool {
        self.workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| tab.panes.values())
            .any(|pane| pane.core.address == address)
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

    fn labeled(address: &str, space: &str, tab: &str, where_label: &str) -> super::CoreTerminal {
        let mut core = super::CoreTerminal {
            address: address.into(),
            state: Some("live".into()),
            cwd: Some("/repo".into()),
            ..super::CoreTerminal::default()
        };
        core.labels.insert("herdr.space".into(), space.into());
        core.labels.insert("herdr.tab".into(), tab.into());
        core.labels.insert("where".into(), where_label.into());
        core
    }

    #[test]
    fn derives_workspaces_and_tabs_from_core_labels() {
        let derived = super::CoreDerivedLayout::derive(vec![
            labeled("t1", "sp1", "tb1", "main / 工作台"),
            labeled("t2", "sp1", "tb1", "main / 工作台"),
            labeled("t3", "sp1", "tb2", "main / second"),
            labeled("t4", "sp2", "tb9", "ops / 值班"),
            super::CoreTerminal {
                address: "wild".into(),
                state: Some("live".into()),
                ..super::CoreTerminal::default()
            },
            super::CoreTerminal {
                address: "dead".into(),
                state: Some("finished".into()),
                ..super::CoreTerminal::default()
            },
        ]);
        assert_eq!(derived.workspaces.len(), 2);
        assert_eq!(derived.workspaces[0].id, "sp1");
        assert_eq!(derived.workspaces[0].name.as_deref(), Some("main"));
        assert_eq!(derived.workspaces[0].tabs.len(), 2);
        assert_eq!(derived.workspaces[0].tabs[0].terminals.len(), 2);
        assert_eq!(
            derived.workspaces[0].tabs[1].name.as_deref(),
            Some("second")
        );
        assert_eq!(derived.workspaces[1].id, "sp2");
        assert_eq!(derived.unorganized.len(), 1);
        assert_eq!(derived.unorganized[0].address, "wild");
    }

    #[test]
    fn derived_tab_layout_contains_every_terminal() {
        let (tab, ids) =
            super::CoreDerivedLayout::build_tab("t".into(), None, 3).expect("non-empty tab");
        assert_eq!(ids.len(), 3);
        assert_eq!(tab.layout.pane_count(), 3);
        assert!(super::CoreDerivedLayout::build_tab("t".into(), None, 0).is_none());
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
