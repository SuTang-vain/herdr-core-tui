//! Core-native Herdr client.
//!
//! This path talks directly to herdr-core and never enters the legacy Herdr
//! server/client autodetection path. It is intentionally independent from the
//! host bridge used by `core_backend`.

mod discovery;
mod events;
mod menu;
mod resume;
mod session;
mod store;
mod ui;
mod workspace_ui;

const CORE_MODE_ENV_VAR: &str = "HERDR_CORE_MODE";

fn mode_is_native(mode: Option<&str>, backend: crate::config::TerminalBackendConfig) -> bool {
    match mode {
        Some("native") => true,
        Some("host") => false,
        _ => backend == crate::config::TerminalBackendConfig::HerdrCore,
    }
}

pub(crate) fn requested(config: &crate::config::Config) -> bool {
    let mode = std::env::var(CORE_MODE_ENV_VAR).ok();
    mode_is_native(mode.as_deref(), config.terminal.backend)
}

pub(crate) fn run() -> std::io::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let _runtime_guard = runtime.enter();
    let discovery = discovery::CoreDiscovery::from_env();
    let terminals = discovery.discover()?;
    workspace_ui::run(discovery, terminals)
}

#[cfg(test)]
mod tests {
    use super::mode_is_native;

    #[test]
    fn core_backend_defaults_to_native_and_host_is_explicit() {
        assert!(mode_is_native(
            None,
            crate::config::TerminalBackendConfig::HerdrCore
        ));
        assert!(mode_is_native(
            Some("native"),
            crate::config::TerminalBackendConfig::Local
        ));
        assert!(!mode_is_native(
            Some("host"),
            crate::config::TerminalBackendConfig::HerdrCore
        ));
        assert!(!mode_is_native(
            None,
            crate::config::TerminalBackendConfig::Local
        ));
    }
}
