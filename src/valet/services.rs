//! Service sockets and durable result forwarding (sections 2, 4, 5 and 6.3).

use std::collections::BTreeMap;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::net::UnixStream;
use tokio::sync::{Notify, RwLock};

use crate::paths;
use crate::protocol::broker::{self, CloseInput, CloseOutput, LookupInput, LookupOutput};
use crate::types::{InteractionId, Outcome};

use super::Valet;
use super::local::{self, ClientKind};

pub(super) struct Services {
    wake: BTreeMap<String, Notify>,
    pub(super) directive_sends: RwLock<()>,
}

impl Services {
    pub(super) fn new<'a>(names: impl Iterator<Item = &'a String>) -> Self {
        Self { wake: names.map(|name| (name.clone(), Notify::new())).collect(), directive_sends: RwLock::new(()) }
    }

    pub(super) fn wake(&self, service: &str) {
        if let Some(wake) = self.wake.get(service) {
            wake.notify_one();
        }
    }
}

pub(super) async fn listen(valet: &Arc<Valet>) -> anyhow::Result<()> {
    let services_dir = valet.data_dir.join("services");
    tokio::task::spawn_blocking(move || DirBuilder::new().recursive(true).mode(0o700).create(&services_dir))
        .await?
        .context("create services directory")?;
    for (name, service) in &valet.profile.services {
        let path = paths::service_config(&valet.data_dir, name);
        let config = service.config.clone().unwrap_or_else(|| serde_json::json!({}));
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let write = || -> anyhow::Result<()> {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&path)?;
                anyhow::ensure!(file.metadata()?.is_file(), "expected a regular file");
                file.set_permissions(Permissions::from_mode(0o600))?;
                file.set_len(0)?;
                file.write_all(&serde_json::to_vec(&config)?)?;
                Ok(())
            };
            write().with_context(|| format!("write {}", path.display()))
        })
        .await??;
        let listener = local::bind(&paths::service_socket(&valet.data_dir, name))?;
        tokio::spawn(local::accept_loop(valet.clone(), listener, ClientKind::Service(name.clone())));
        tokio::spawn(forward_loop(valet.clone(), name.clone()));
    }
    Ok(())
}

pub(super) async fn check_peer(stream: &UnixStream, owner: &str, service: &str) -> anyhow::Result<()> {
    let pid = stream.peer_cred().context("read SO_PEERCRED")?.pid().filter(|pid| *pid > 0).context("no peer pid")?;
    let path = format!("/proc/{pid}/cgroup");
    let cgroups = tokio::fs::read_to_string(&path).await.with_context(|| format!("read {path}"))?;
    let expected = paths::service_unit(owner, service);
    let actual = service_unit(&cgroups);
    if actual != Some(expected.as_str()) {
        bail!("pid {pid} is in unit {actual:?}, expected {expected}");
    }
    Ok(())
}

fn service_unit(cgroups: &str) -> Option<&str> {
    let path = cgroups.lines().find_map(|line| line.strip_prefix("0::/"))?;
    path.rsplit('/').find(|component| component.ends_with(".service"))
}

/// Check the write-ahead sends while holding the service-directive submit lock.
/// If an accepted send's local success write failed, settle it by broker lookup.
pub(super) async fn director(valet: &Valet, cause: InteractionId) -> anyhow::Result<Option<String>> {
    let _directives = valet.services.directive_sends.write().await;
    let (service, unfinished) = valet
        .store
        .with(move |s| -> crate::store::Result<_> {
            let service = s.service_director(cause)?;
            let unfinished = if service.is_none() { s.unfinished_sends()? } else { Vec::new() };
            Ok((service, unfinished))
        })
        .await?;
    for send in unfinished {
        let Some(name) = send.service else { continue };
        if send.input["kind"] != "directive" {
            continue;
        }
        let found: LookupOutput = valet.broker.request(broker::LOOKUP, &LookupInput { client_ref: send.id }).await?;
        if let Some(reply) = found.interaction.filter(|reply| found.found && reply.id == cause) {
            valet.store.with(move |s| s.recover_send(send.id, &reply)).await?;
            return Ok(Some(name));
        }
    }
    Ok(service)
}

async fn forward_loop(valet: Arc<Valet>, service: String) {
    valet.wait_recovered().await;
    let wake = &valet.services.wake[&service];
    loop {
        let retry = match forward_pending(&valet, &service).await {
            Ok(retry) => retry,
            Err(e) => {
                tracing::warn!(service, "could not forward service results: {e:#}");
                true
            }
        };
        if retry {
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        } else {
            wake.notified().await;
        }
    }
}

/// Forward newly received results and retry closes after reconnect or restart.
async fn forward_pending(valet: &Valet, service: &str) -> anyhow::Result<bool> {
    let name = service.to_owned();
    let forwarded_only = !valet.clients.service_connected(service);
    let pending = valet.store.with(move |s| s.pending_service_results(&name, forwarded_only)).await?;
    let mut retry = false;
    for (receive, interaction, forwarded) in pending {
        let id = interaction.id;
        if !forwarded {
            match valet.clients.notify_service(interaction, service).await {
                Some(true) => valet.store.with(move |s| s.mark_service_result_forwarded(receive)).await?,
                Some(false) => return Ok(true),
                None => return Ok(retry),
            }
        }
        let close = CloseInput { id, outcome: Outcome::Received };
        match valet.broker.request::<_, CloseOutput>(broker::CLOSE, &close).await {
            Ok(_) => valet.store.with(move |s| s.finish_service_result(receive)).await?,
            Err(e) => {
                tracing::warn!(%id, service, "service result close failed: {e}");
                retry = true;
            }
        }
    }
    Ok(retry)
}

#[cfg(test)]
mod tests {
    use super::service_unit;

    #[test]
    fn parses_innermost_v2_service_unit() {
        let unit = "gllmh-service-dispatch::draft_dispatcher.service";
        for path in [
            format!("/system.slice/{unit}"),
            format!("/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}"),
            format!("/system.slice/{unit}/nested/child.scope"),
        ] {
            assert_eq!(service_unit(&format!("1:name=systemd:/other.service\n0::{path}\n")), Some(unit));
        }
        assert_eq!(
            service_unit("0::/system.slice/gllmh-service-alice::draft_dispatcher.service\n"),
            Some("gllmh-service-alice::draft_dispatcher.service")
        );
        assert_eq!(
            service_unit("0::/system.slice/gllmh-valet-dispatch.service/tool.scope\n"),
            Some("gllmh-valet-dispatch.service")
        );
        assert_eq!(service_unit(&format!("0::/system.slice/{unit}/nested/other.service\n")), Some("other.service"));
        assert_eq!(service_unit("0::/user.slice/user-1000.slice/session-1.scope\n"), None);
        assert_eq!(
            service_unit("1:name=systemd:/system.slice/example.service\n2:cpu:/system.slice/example.service\n"),
            None
        );
        assert_eq!(service_unit(""), None);
    }
}
