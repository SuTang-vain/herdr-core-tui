//! Experimental herdr-core transport for pane commands.
//!
//! Core owns the real terminal and agent process. Herdr's local PTY only runs
//! `hc attach`, so the existing layout, renderer, and input stack stay intact.

use std::ffi::OsString;
use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use portable_pty::CommandBuilder;

use crate::layout::PaneId;

const BACKEND_ENV_VAR: &str = "HERDR_BACKEND";
const CORE_BACKEND: &str = "herdr-core";
const CORE_CLI_ENV_VAR: &str = "HERDR_CORE_CLI";
static CORE_CONFIGURED: AtomicBool = AtomicBool::new(false);

pub(crate) fn configure(backend: crate::config::TerminalBackendConfig) {
    CORE_CONFIGURED.store(
        backend == crate::config::TerminalBackendConfig::HerdrCore,
        Ordering::Release,
    );
}

fn enabled(command: &CommandBuilder) -> bool {
    CORE_CONFIGURED.load(Ordering::Acquire)
        || command
            .get_env(BACKEND_ENV_VAR)
            .is_some_and(|value| value == CORE_BACKEND)
}

fn core_cli() -> OsString {
    std::env::var_os(CORE_CLI_ENV_VAR).unwrap_or_else(|| OsString::from("hc"))
}

fn should_forward_to_core(key: &str) -> bool {
    !matches!(
        key,
        "HERDR_ENV"
            | "HERDR_BIN_PATH"
            | "HERDR_SOCKET_PATH"
            | "HERDR_CLIENT_SOCKET_PATH"
            | "HERDR_PANE_ID"
            | "HERDR_TAB_ID"
            | "HERDR_WORKSPACE_ID"
            | "HERDR_PANE_RUNTIME_ID"
            | "HERDR_BACKEND"
            | "HERDR_CORE_CLI"
            | "HC_TERMINAL_ID"
    )
}

fn parse_core_address(output: &[u8]) -> io::Result<String> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let address = value
        .get("address")
        .and_then(serde_json::Value::as_str)
        .filter(|address| !address.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "hc run response did not contain a terminal address",
            )
        })?;
    Ok(address.to_string())
}

fn create_core_terminal(pane_id: PaneId, command: &CommandBuilder) -> io::Result<String> {
    let mut run = Command::new(core_cli());
    run.arg("run")
        .arg("--json")
        .arg("--bg")
        .arg("--keep")
        .arg("--no-parent")
        .arg("--no-notify")
        .arg("--label")
        .arg("herdr.backend=herdr-core")
        .arg("--label")
        .arg(format!("herdr.pane={}", pane_id.raw()));
    if let Some(cwd) = command.get_cwd() {
        run.arg("--cwd").arg(cwd);
    }
    let argv = command.get_argv();
    if !argv.is_empty() {
        run.arg("--").args(argv);
    }
    for (key, value) in command.iter_full_env_as_str() {
        if should_forward_to_core(key) {
            run.env(key, value);
        } else {
            run.env_remove(key);
        }
    }
    let output = run.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "hc run failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_core_address(&output.stdout)
}

pub(crate) fn prepare_pane_command(
    pane_id: PaneId,
    command: CommandBuilder,
) -> io::Result<CommandBuilder> {
    if !enabled(&command) {
        return Ok(command);
    }

    let address = create_core_terminal(pane_id, &command)?;
    let mut attach = CommandBuilder::new(core_cli());
    attach.arg("attach");
    attach.arg("--history");
    attach.arg("200");
    attach.arg(address);
    if let Some(cwd) = command.get_cwd() {
        attach.cwd(cwd);
    }
    for (key, value) in command.iter_full_env_as_str() {
        attach.env(key, value);
    }
    Ok(attach)
}

#[cfg(test)]
mod tests {
    use super::{parse_core_address, should_forward_to_core};

    #[test]
    fn parses_core_terminal_address() {
        assert_eq!(
            parse_core_address(br#"{"address":"t12","status":"idle"}"#)
                .expect("valid response")
                .as_str(),
            "t12"
        );
        assert!(parse_core_address(br#"{"status":"idle"}"#).is_err());
        assert!(parse_core_address(b"not json").is_err());
    }

    #[test]
    fn does_not_leak_tui_or_parent_core_identity_into_core() {
        for key in [
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_PANE_ID",
            "HERDR_BACKEND",
            "HC_TERMINAL_ID",
        ] {
            assert!(!should_forward_to_core(key), "unexpectedly forwarded {key}");
        }
        for key in ["PATH", "HOME", "KIMI_CODE_HOME", "COLORTERM"] {
            assert!(should_forward_to_core(key), "incorrectly removed {key}");
        }
    }
}
