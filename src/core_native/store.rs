use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::persist::LayoutSnapshot;

pub(crate) const CORE_NATIVE_STORE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub(crate) struct CoreSessionSnapshot {
    #[serde(default = "store_version")]
    pub version: u32,
    #[serde(default)]
    pub active_workspace: usize,
    #[serde(default)]
    pub workspaces: Vec<CoreWorkspaceSnapshot>,
}

impl Default for CoreSessionSnapshot {
    fn default() -> Self {
        Self {
            version: CORE_NATIVE_STORE_VERSION,
            active_workspace: 0,
            workspaces: Vec::new(),
        }
    }
}

fn store_version() -> u32 {
    CORE_NATIVE_STORE_VERSION
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CoreWorkspaceSnapshot {
    pub id: String,
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub active_tab: usize,
    pub tabs: Vec<CoreTabSnapshot>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CoreTabSnapshot {
    pub id: String,
    pub name: Option<String>,
    pub layout: LayoutSnapshot,
    pub focused: u32,
    pub zoomed: bool,
    pub panes: HashMap<u32, CorePaneSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CorePaneSnapshot {
    pub core_address: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub agent_session: Option<CoreAgentSessionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoreAgentSessionSnapshot {
    pub agent: String,
    pub session_id: String,
    pub resume_argv: Vec<String>,
}

pub(crate) fn store_path() -> PathBuf {
    crate::config::config_dir().join("core-native-session.json")
}

pub(crate) fn load_from(path: &Path) -> io::Result<CoreSessionSnapshot> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid core-native session store {}: {err}",
                    path.display()
                ),
            )
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(CoreSessionSnapshot::default()),
        Err(err) => Err(err),
    }
}

pub(crate) fn load() -> io::Result<CoreSessionSnapshot> {
    load_from(&store_path())
}

pub(crate) fn save_to(path: &Path, snapshot: &CoreSessionSnapshot) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(snapshot)?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

pub(crate) fn save(snapshot: &CoreSessionSnapshot) -> io::Result<()> {
    save_to(&store_path(), snapshot)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{load_from, save_to, CorePaneSnapshot, CoreSessionSnapshot};
    use crate::persist::{DirectionSnapshot, LayoutSnapshot};

    fn test_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "herdr-core-native-store-{}-{nonce}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_store_is_an_empty_native_session() {
        let snapshot = load_from(&test_path()).expect("missing store");
        assert!(snapshot.workspaces.is_empty());
        assert_eq!(snapshot.version, CORE_NATIVE_STORE_VERSION);
    }

    #[test]
    fn store_round_trip_preserves_layout_and_core_addresses() {
        let path = test_path();
        let mut panes = HashMap::new();
        for (id, address) in [(7, "t42"), (8, "t43")] {
            panes.insert(
                id,
                CorePaneSnapshot {
                    core_address: address.into(),
                    cwd: "/repo".into(),
                    agent_session: None,
                },
            );
        }
        let snapshot = CoreSessionSnapshot {
            version: CORE_NATIVE_STORE_VERSION,
            active_workspace: 0,
            workspaces: vec![super::CoreWorkspaceSnapshot {
                id: "w1".into(),
                name: None,
                cwd: "/repo".into(),
                active_tab: 0,
                tabs: vec![super::CoreTabSnapshot {
                    id: "t1".into(),
                    name: None,
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(7)),
                        second: Box::new(LayoutSnapshot::Pane(8)),
                    },
                    focused: 7,
                    zoomed: false,
                    panes,
                }],
            }],
        };
        save_to(&path, &snapshot).expect("save store");
        let restored = load_from(&path).expect("load store");
        assert_eq!(
            restored.workspaces[0].tabs[0].panes.get(&7),
            Some(&CorePaneSnapshot {
                core_address: "t42".into(),
                cwd: "/repo".into(),
                agent_session: None,
            })
        );
        std::fs::remove_file(path).expect("remove test store");
    }

    #[test]
    fn invalid_store_is_an_error_instead_of_a_silent_reset() {
        let path = test_path();
        std::fs::write(&path, b"not json").expect("write invalid store");
        assert!(load_from(&path).is_err());
        std::fs::remove_file(path).expect("remove test store");
    }

    use super::CORE_NATIVE_STORE_VERSION;
}
