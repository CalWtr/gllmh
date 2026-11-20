//! `sd_notify(READY=1)` without libsystemd, for the broker and the valet
//! (both `Type=notify`, section 7 Unit files). A no-op outside systemd.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

pub const ENV_NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";

/// Tell systemd the unit is ready. Returns Ok(false) when not under systemd.
pub fn ready() -> std::io::Result<bool> {
    notify("READY=1")
}

/// Send one notification state string to $NOTIFY_SOCKET.
pub fn notify(state: &str) -> std::io::Result<bool> {
    let Some(path) = std::env::var_os(ENV_NOTIFY_SOCKET) else {
        return Ok(false);
    };
    notify_to(&path.to_string_lossy(), state)?;
    Ok(true)
}

/// Send one state string to the given notify socket (`@name` = abstract).
pub fn notify_to(path: &str, state: &str) -> std::io::Result<()> {
    let addr = match path.strip_prefix('@') {
        Some(abstract_name) => SocketAddr::from_abstract_name(abstract_name.as_bytes())?,
        None => SocketAddr::from_pathname(path)?,
    };
    let sock = UnixDatagram::unbound()?;
    sock.send_to_addr(state.as_bytes(), &addr)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_ready_to_notify_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let rx = UnixDatagram::bind(&path).unwrap();
        notify_to(path.to_str().unwrap(), "READY=1").unwrap();
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }
}
