use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Context, ensure};

use crate::paths;
use crate::profile::LoadedRuntime;

pub(super) fn prepare(loaded: &mut LoadedRuntime, bin: &Path, dev: bool) -> anyhow::Result<()> {
    executable(bin.to_str().context("--bin must be UTF-8")?)?;
    ensure!(single_line(&loaded.runtime.name), "runtime name cannot be represented in a unit description");
    let caller_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    for (name, user) in &mut loaded.users {
        unit_name(name, &format!("gllmh-valet-{name}.service"))?;
        ensure!(
            !user.system_user.is_empty() && single_line(&user.system_user),
            "invalid system_user {:?} for unit generation",
            user.system_user
        );

        let paths = std::iter::once(&user.user_data_store)
            .chain(user.cwd.as_ref())
            .chain(user.services.values().filter_map(|service| service.cwd.as_ref()));
        if paths.into_iter().any(|path| path.starts_with("~")) {
            let home = if dev {
                caller_home.clone()
            } else {
                passwd_home(&user.system_user).with_context(|| format!("resolve home for {:?}", user.system_user))?
            };
            for path in std::iter::once(&mut user.user_data_store)
                .chain(user.cwd.as_mut())
                .chain(user.services.values_mut().filter_map(|service| service.cwd.as_mut()))
            {
                *path = paths::expand_home(path, &home);
            }
        }

        check_working_dir(user.cwd.as_deref().unwrap_or(&user.user_data_store))?;
        ensure!(user.user_data_store.to_str().is_some(), "user_data_store must be UTF-8");
        for (service_name, service) in &user.services {
            unit_name(service_name, &paths::service_unit(name, service_name))?;
            executable(&service.command).with_context(|| format!("service {name}::{service_name}"))?;
            ensure!(service.args.iter().all(|arg| !arg.contains('\0')), "service {name}::{service_name}: NUL in args");
            if let Some(cwd) = &service.cwd {
                check_working_dir(cwd)?;
            }
        }
    }
    Ok(())
}

fn unit_name(name: &str, unit: &str) -> anyhow::Result<()> {
    ensure!(
        unit.len() <= 255 && name.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"_.-".contains(&ch)),
        "{name:?} cannot be used unchanged in systemd unit name {unit:?}"
    );
    Ok(())
}

fn executable(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty()
            && !value.contains('\0')
            && (value.starts_with('/')
                || (!value.contains('/') && !value.starts_with(['-', '@', '|', ':', '+', '!', ';']))),
        "{value:?} must be an absolute executable path or a bare executable name"
    );
    Ok(())
}

fn check_working_dir(path: &Path) -> anyhow::Result<()> {
    ensure!(
        path.is_absolute() && path.to_str().is_some_and(single_line),
        "WorkingDirectory={path:?} must be an absolute UTF-8 path representable on one unit line"
    );
    Ok(())
}

// These directives are parsed as single line values, without C-style unquoting.
fn single_line(value: &str) -> bool {
    value.trim() == value && !value.ends_with('\\') && !value.chars().any(char::is_control)
}

fn passwd_home(user: &str) -> io::Result<PathBuf> {
    let user = CString::new(user)?;
    let mut buffer = vec![0; 1024];
    loop {
        let mut record = MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        // SAFETY: The pointers refer to valid input and writable storage for this call.
        let status = unsafe {
            libc::getpwnam_r(user.as_ptr(), record.as_mut_ptr(), buffer.as_mut_ptr(), buffer.len(), &mut result)
        };
        match status {
            0 if result.is_null() => return Err(io::Error::new(io::ErrorKind::NotFound, "no passwd entry")),
            0 => {
                // SAFETY: A successful lookup with a non-null result initializes record
                // and points pw_dir at a NUL-terminated string in the live buffer.
                let home = unsafe { CStr::from_ptr(record.assume_init().pw_dir) };
                return Ok(PathBuf::from(OsStr::from_bytes(home.to_bytes())));
            }
            libc::ERANGE => buffer.resize(buffer.len() * 2, 0),
            code => return Err(io::Error::from_raw_os_error(code)),
        }
    }
}
