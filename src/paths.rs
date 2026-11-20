//! Fixed paths and file names of spec section 5 (Who owns what), plus the
//! dev-mode overrides. Every module takes its paths from here.

use std::path::{Path, PathBuf};

/// Profiles, one directory per type (section 7). Only the broker reads them.
pub const DEFAULT_CONFIG_DIR: &str = "/etc/gllmh";
/// Env override for the config dir (dev and tests).
pub const ENV_CONFIG_DIR: &str = "GLLMH_CONFIG_DIR";
/// One socket per user, created by systemd (or the broker in dev mode).
pub const DEFAULT_RUN_DIR: &str = "/run/gllmh/users";
/// Env override for the user socket dir.
pub const ENV_RUN_DIR: &str = "GLLMH_RUN_DIR";
pub const DEFAULT_BROKER_DB: &str = "/var/lib/gllmh/broker.db";
/// `user_profile.user_data_store` default, relative to the system user's home.
pub const DEFAULT_USER_DATA_STORE: &str = "~/.gllmh";
/// Default binary path in generated units.
pub const DEFAULT_BIN: &str = "/usr/bin/gllmh";

pub const STORE_DB: &str = "store.db";
pub const LOCAL_SOCK: &str = "local.sock";
pub const SERVICES_DIR: &str = "services";

/// Profile directories under the config dir, one per type.
pub mod profile_dirs {
    pub const RUNTIME: &str = "runtime";
    pub const USER: &str = "user";
    pub const LLM: &str = "llm";
    pub const COMPOSER: &str = "composer";
    pub const TOOL: &str = "tool";
}

/// `--config-dir`, shared by the commands that read profiles.
#[derive(Debug, Clone, clap::Args)]
pub struct ConfigDirArg {
    /// Directory holding the profiles, one subdirectory per profile type.
    #[arg(long = "config-dir", env = ENV_CONFIG_DIR, default_value = DEFAULT_CONFIG_DIR)]
    pub config_dir: PathBuf,
}

/// `--run-dir`, shared by the broker and the valet.
#[derive(Debug, Clone, clap::Args)]
pub struct RunDirArg {
    /// Directory of the per-user broker sockets.
    #[arg(long = "run-dir", env = ENV_RUN_DIR, default_value = DEFAULT_RUN_DIR)]
    pub run_dir: PathBuf,
}

/// `<run_dir>/<user>.sock`.
pub fn user_socket(run_dir: &Path, user: &str) -> PathBuf {
    run_dir.join(format!("{user}.sock"))
}

/// `<user_data_store>/store.db`.
pub fn store_db(user_data_store: &Path) -> PathBuf {
    user_data_store.join(STORE_DB)
}

/// `<user_data_store>/local.sock`.
pub fn local_socket(user_data_store: &Path) -> PathBuf {
    user_data_store.join(LOCAL_SOCK)
}

/// `<user_data_store>/services/<name>.sock`.
pub fn service_socket(user_data_store: &Path, service: &str) -> PathBuf {
    user_data_store.join(SERVICES_DIR).join(format!("{service}.sock"))
}

/// `<user_data_store>/services/<name>.json`.
pub fn service_config(user_data_store: &Path, service: &str) -> PathBuf {
    user_data_store.join(SERVICES_DIR).join(format!("{service}.json"))
}

/// systemd unit name of a service: `gllmh-service-<owner>::<service>.service`.
pub fn service_unit(owner: &str, service: &str) -> String {
    format!("gllmh-service-{owner}::{service}.service")
}

/// Expand a leading `~` against `home`.
pub fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// The default local.sock of the calling user: `$HOME/.gllmh/local.sock`.
pub fn default_local_socket() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    local_socket(&expand_home(Path::new(DEFAULT_USER_DATA_STORE), &home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        assert_eq!(expand_home(Path::new("~/.gllmh"), Path::new("/home/a")), PathBuf::from("/home/a/.gllmh"));
        assert_eq!(expand_home(Path::new("/srv/x"), Path::new("/home/a")), PathBuf::from("/srv/x"));
        assert_eq!(service_unit("agent", "timer"), "gllmh-service-agent::timer.service");
    }
}
