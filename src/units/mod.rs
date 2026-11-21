//! systemd unit generation of spec section 7 (Unit files): `gllmh build`.
//!
//! Owner: M1.2.

mod prepare;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::paths::{self, ConfigDirArg, DEFAULT_BIN, DEFAULT_RUN_DIR};
use crate::profile::{self, LoadedRuntime, Restart};

/// `gllmh build --profile <name>`.
#[derive(Debug, Clone, clap::Args)]
pub struct BuildArgs {
    /// The runtime_profile to turn into units.
    #[arg(long)]
    pub profile: String,
    /// Write one file per unit into this directory instead of printing them.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Path of the gllmh binary in ExecStart= lines.
    #[arg(long, default_value = DEFAULT_BIN)]
    pub bin: PathBuf,
    /// Skip account and file-mode validation and expand home paths using the caller's HOME.
    #[arg(long)]
    pub dev: bool,
    #[command(flatten)]
    pub config: ConfigDirArg,
}

pub async fn build(args: BuildArgs) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut loaded = match profile::validate(&args.config.config_dir, &args.profile, args.dev) {
            Ok(loaded) => loaded,
            Err(violations) => {
                for violation in &violations {
                    eprintln!("{violation}");
                }
                anyhow::bail!("profile validation failed ({} violation(s))", violations.len());
            }
        };
        prepare::prepare(&mut loaded, &args.bin, args.dev)?;
        let units = generate(&loaded, &args.bin);
        if let Some(out) = args.out {
            std::fs::create_dir_all(&out).with_context(|| format!("create unit directory {}", out.display()))?;
            for (name, contents) in units {
                let path = out.join(name);
                std::fs::write(&path, contents).with_context(|| format!("write unit {}", path.display()))?;
            }
        } else {
            let mut stdout = io::stdout().lock();
            for (index, (name, contents)) in units.into_iter().enumerate() {
                if index > 0 {
                    writeln!(stdout)?;
                }
                write!(stdout, "# {name}\n{contents}")?;
            }
            stdout.flush()?;
        }
        Ok(())
    })
    .await?
}

/// Render section 7 unit files from a validated runtime with expanded home paths.
/// Paths must be UTF-8 and names and paths must be representable in systemd directives.
pub fn generate(loaded: &LoadedRuntime, bin: &Path) -> Vec<(String, String)> {
    let profile = &loaded.runtime;
    let description = literal(&profile.name);
    let bin = word(&bin.to_string_lossy());
    let runtime_restart = restart(profile.restart);
    let grace = profile.child_process_shutdown_grace_s;
    let mut units = vec![("gllmh.target".into(), format!("[Unit]\nDescription=GLLMH runtime {description}\n"))];

    let mut sockets = Vec::new();
    for (name, user) in &loaded.users {
        let socket_unit = format!("gllmh-user-{name}.socket");
        let socket = paths::user_socket(Path::new(DEFAULT_RUN_DIR), name);
        let system_user = literal(&user.system_user);
        let socket = literal(&socket.to_string_lossy());
        units.push((
            socket_unit.clone(),
            format!(
                "[Unit]\nPartOf=gllmh.target\n\n\
             [Socket]\nListenStream={socket}\nSocketUser={system_user}\nSocketMode=0600\n\
             FileDescriptorName={name}\nService=gllmh-broker.service\n\n\
             [Install]\nWantedBy=gllmh.target\n"
            ),
        ));
        sockets.push(socket_unit);
    }

    let sockets = sockets.join(" ");
    let profile_arg = exec_arg(&profile.name);
    units.push((
        "gllmh-broker.service".into(),
        format!(
            "[Unit]\nDescription=GLLMH broker\nPartOf=gllmh.target\nRequires={sockets}\n\n\
         [Service]\nType=notify\nUser=gllmh\nExecStart={bin} serve --profile {profile_arg}\n\
         Sockets={sockets}\nStateDirectory=gllmh\nRestart={runtime_restart}\nTimeoutStopSec={grace}\n\
         NoNewPrivileges=yes\n\n[Install]\nWantedBy=gllmh.target\n"
        ),
    ));

    for (name, user) in &loaded.users {
        let valet = format!("gllmh-valet-{name}.service");
        let user_arg = exec_arg(name);
        let system_user = literal(&user.system_user);
        let working_dir = working_directory(user.cwd.as_deref().unwrap_or(&user.user_data_store));
        units.push((
            valet.clone(),
            format!(
                "[Unit]\nDescription=GLLMH valet for {name}\nPartOf=gllmh.target\n\
             Wants=gllmh-broker.service\nAfter=gllmh-broker.service\n\n\
             [Service]\nType=notify\nUser={system_user}\nExecStart={bin} valet --user {user_arg}\n\
             {working_dir}Restart={runtime_restart}\nTimeoutStopSec={grace}\nNoNewPrivileges=yes\n\n\
             [Install]\nWantedBy=gllmh.target\n"
            ),
        ));

        for (service_name, service) in &user.services {
            let command = std::iter::once(word(&service.command))
                .chain(service.args.iter().map(|arg| exec_arg(arg)))
                .collect::<Vec<_>>()
                .join(" ");
            let working_dir = service.cwd.as_deref().map(working_directory).unwrap_or_default();
            let socket = paths::service_socket(&user.user_data_store, service_name);
            let config = paths::service_config(&user.user_data_store, service_name);
            let socket = word(&format!("GLLMH_SERVICE_SOCKET={}", socket.display()));
            let config = word(&format!("GLLMH_SERVICE_CONFIG={}", config.display()));
            // Upheld, not required: a restart of the service must not propagate to the
            // target and from there to every valet (docs/decisions.md, A4 follow-up).
            let install = if service.required { "UpheldBy" } else { "WantedBy" };
            let restart = restart(service.restart);
            units.push((
                paths::service_unit(name, service_name),
                format!(
                    "[Unit]\nDescription=GLLMH service {name}::{service_name}\nPartOf=gllmh.target\n\
                 BindsTo={valet}\nAfter={valet}\n\n\
                 [Service]\nUser={system_user}\nExecStart={command}\n{working_dir}\
                 Environment={socket}\nEnvironment={config}\nRestart={restart}\n\n\
                 [Install]\n{install}=gllmh.target\n"
                ),
            ));
        }
    }

    units
}

fn restart(restart: Restart) -> &'static str {
    match restart {
        Restart::OnFailure => "on-failure",
        Restart::Never => "no",
    }
}

fn working_directory(path: &Path) -> String {
    format!("WorkingDirectory={}\n", literal(&path.to_string_lossy()))
}

fn literal(value: &str) -> String {
    value.replace('%', "%%")
}

fn exec_arg(value: &str) -> String {
    word(&value.replace('$', "$$"))
}

// Quote a word for systemd's C-style parser, used by ExecStart and Environment.
fn word(value: &str) -> String {
    let value = literal(value);
    if !value.is_empty()
        && value != ";"
        && !value.chars().any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '\\' | '"' | '\''))
    {
        return value;
    }

    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            ch if ch.is_ascii_control() => {
                use std::fmt::Write;
                write!(quoted, "\\x{:02x}", ch as u32).expect("writing to a String cannot fail");
            }
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}
