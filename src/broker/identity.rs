//! Connection → user resolution (spec section 2).
//!
//! Owner: M2.2.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixListener;
use std::ptr;

use anyhow::{Context, ensure};

use crate::profile::LoadedRuntime;

/// The listener's verified name is the sole source of an accepted connection's user.
pub(super) fn connection_user(listener_user: &str) -> &str {
    listener_user
}

/// Adopt the named listening sockets systemd passed (sections 2, 5 and 7).
pub(super) fn inherited_listeners(profile: &LoadedRuntime) -> anyhow::Result<Vec<(String, UnixListener)>> {
    if let Some(pid) = std::env::var_os("LISTEN_PID") {
        ensure!(
            pid.to_str().and_then(|pid| pid.parse().ok()) == Some(std::process::id()),
            "LISTEN_PID does not name this process"
        );
    }
    let count =
        std::env::var("LISTEN_FDS").context("missing LISTEN_FDS")?.parse::<RawFd>().context("invalid LISTEN_FDS")?;
    let names = std::env::var("LISTEN_FDNAMES").context("missing LISTEN_FDNAMES")?;
    let names = match_names(count, &names, profile.users.keys().map(String::as_str))?;
    names
        .into_iter()
        .zip(3..)
        .map(|(user, fd)| {
            let listener = adopt_listener(fd).with_context(|| format!("inherited socket {user:?} (fd {fd})"))?;
            let account = &profile.users[user].system_user;
            let expected = user_id(account).with_context(|| format!("look up system user {account:?}"))?;
            verify_socket(&listener, expected)
                .with_context(|| format!("inherited socket for {user:?} ({account:?})"))?;
            listener.set_nonblocking(true)?;
            Ok((user.to_owned(), listener))
        })
        .collect()
}

fn verify_socket(listener: &UnixListener, expected_uid: u32) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    let path = addr.as_pathname().context("inherited socket has no filesystem path")?;
    let metadata = fs::symlink_metadata(path).with_context(|| format!("stat inherited socket {path:?}"))?;
    ensure!(metadata.file_type().is_socket(), "inherited socket path {path:?} is not a socket");
    ensure!(
        metadata.uid() == expected_uid,
        "inherited socket {path:?} is owned by UID {}, expected UID {expected_uid}",
        metadata.uid()
    );
    ensure!(metadata.mode() & 0o7777 == 0o600, "inherited socket {path:?} must have mode 0600");
    Ok(())
}

fn match_names<'a>(
    count: RawFd,
    names: &'a str,
    expected: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<Vec<&'a str>> {
    let names: Vec<_> = if names.is_empty() { Vec::new() } else { names.split(':').collect() };
    ensure!(
        (0..=RawFd::MAX - 3).contains(&count) && names.len() == count as usize,
        "LISTEN_FDS and LISTEN_FDNAMES disagree"
    );
    let expected: BTreeSet<_> = expected.into_iter().collect();
    let mut seen = BTreeSet::new();
    for &name in &names {
        ensure!(expected.contains(name), "unknown inherited socket name {name:?}");
        ensure!(seen.insert(name), "duplicate inherited socket name {name:?}");
    }
    let missing: Vec<_> = expected.difference(&seen).collect();
    ensure!(missing.is_empty(), "missing inherited sockets for {missing:?}");
    Ok(names)
}

fn adopt_listener(fd: RawFd) -> anyhow::Result<UnixListener> {
    ensure!(socket_option(fd, libc::SO_DOMAIN)? == libc::AF_UNIX, "fd is not a Unix socket");
    ensure!(socket_option(fd, libc::SO_TYPE)? == libc::SOCK_STREAM, "fd is not a stream socket");
    ensure!(socket_option(fd, libc::SO_ACCEPTCONN)? == 1, "fd is not a listening socket");
    // SAFETY: fcntl takes an integer argument for F_SETFD.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: systemd transfers ownership of each validated, distinct listening fd once.
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

fn socket_option(fd: RawFd, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut size = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt validates the raw fd and writes into the supplied storage.
    let result =
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, option, (&mut value as *mut libc::c_int).cast(), &mut size) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn user_id(name: &str) -> io::Result<u32> {
    let name = CString::new(name)?;
    let mut buffer = vec![0; 1024];
    loop {
        let mut entry = MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        // SAFETY: all outputs have valid storage and name is a NUL-terminated string.
        let code = unsafe {
            libc::getpwnam_r(name.as_ptr(), entry.as_mut_ptr(), buffer.as_mut_ptr(), buffer.len(), &mut result)
        };
        match code {
            0 if result.is_null() => return Err(io::Error::new(io::ErrorKind::NotFound, "no passwd entry")),
            // SAFETY: success and a non-null result indicate an initialized record.
            0 => return Ok(unsafe { entry.assume_init() }.pw_uid),
            libc::ERANGE => buffer.resize(buffer.len() * 2, 0),
            code => return Err(io::Error::from_raw_os_error(code)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn matches_systemd_names_in_descriptor_order() {
        let expected = ["agent", "user"];
        assert_eq!(match_names(2, "user:agent", expected).unwrap(), ["user", "agent"]);
        for (count, names, error) in [
            (1, "user", "missing"),
            (2, "user", "disagree"),
            (2, "user:other", "unknown"),
            (2, "user:user", "duplicate"),
            (2, "user:", "unknown"),
            (-1, "", "disagree"),
        ] {
            assert!(match_names(count, names, expected).unwrap_err().to_string().contains(error));
        }
    }

    #[test]
    fn checks_the_owner_and_mode_of_the_inherited_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let listener = adopt_listener(listener.into_raw_fd()).unwrap();
        let owner = fs::metadata(&path).unwrap().uid();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        verify_socket(&listener, owner).unwrap();
        assert!(verify_socket(&listener, owner.wrapping_add(1)).unwrap_err().to_string().contains("UID"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o660)).unwrap();
        assert!(verify_socket(&listener, owner).unwrap_err().to_string().contains("0600"));
    }
}
