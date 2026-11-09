use std::{collections::HashMap, io, net::IpAddr};

use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::net::lookup_host;
use tokio::task::{JoinHandle, spawn_local};

use tokio_util::sync::CancellationToken;

use lan_mouse_ipc::ClientHandle;

pub(crate) struct DnsResolver {
    cancellation_token: CancellationToken,
    task: Option<JoinHandle<()>>,
    request_tx: Sender<DnsRequest>,
    event_rx: Receiver<DnsEvent>,
}

struct DnsRequest {
    handle: ClientHandle,
    hostname: String,
}

pub(crate) enum DnsEvent {
    Resolving(ClientHandle),
    Resolved(ClientHandle, String, io::Result<Vec<IpAddr>>),
}

struct DnsTask {
    request_rx: Receiver<DnsRequest>,
    event_tx: Sender<DnsEvent>,
    cancellation_token: CancellationToken,
    active_tasks: HashMap<ClientHandle, JoinHandle<()>>,
}

impl DnsResolver {
    pub(crate) fn new() -> io::Result<Self> {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let dns_task = DnsTask {
            active_tasks: Default::default(),
            request_rx,
            event_tx,
            cancellation_token: cancellation_token.clone(),
        };
        let task = Some(spawn_local(dns_task.run()));
        Ok(Self {
            cancellation_token,
            task,
            event_rx,
            request_tx,
        })
    }

    pub(crate) fn resolve(&self, handle: ClientHandle, hostname: String) {
        let request = DnsRequest { handle, hostname };
        self.request_tx.send(request).ok().unwrap_or_default();
    }

    pub(crate) async fn event(&mut self) -> DnsEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        // Idempotent: a second shutdown (or a task that already panicked)
        // must not abort the process, which runs with `panic = "abort"` in
        // release builds.
        if let Some(task) = self.task.take() {
            if let Err(e) = task.await {
                log::warn!("dns resolver task did not shut down cleanly: {e}");
            }
        }
    }
}

impl DnsTask {
    async fn run(mut self) {
        let cancellation_token = self.cancellation_token.clone();
        tokio::select! {
            _ = self.do_dns() => {},
            _ = cancellation_token.cancelled() => {},
        }
    }

    async fn do_dns(&mut self) {
        while let Some(dns_request) = self.request_rx.recv().await {
            let DnsRequest { handle, hostname } = dns_request;

            // Finished resolutions of deleted clients would otherwise stay in
            // the map for the whole service lifetime (handles are never
            // reused by the slab allocator in a strict order, so this map
            // could grow with every create/delete cycle).
            self.active_tasks.retain(|_, task| !task.is_finished());

            /* abort previous dns task */
            let previous_task = self.active_tasks.remove(&handle);
            if let Some(task) = previous_task {
                if !task.is_finished() {
                    task.abort();
                }
            }

            self.event_tx
                .send(DnsEvent::Resolving(handle))
                .ok()
                .unwrap_or_default();

            /* spawn task for dns request */
            let event_tx = self.event_tx.clone();
            let cancellation_token = self.cancellation_token.clone();

            let task = tokio::task::spawn_local(async move {
                tokio::select! {
                    result = resolve_hostname(&hostname) => {
                       event_tx
                           .send(DnsEvent::Resolved(handle, hostname, result))
                           .ok().unwrap_or_default();
                    }
                    _ = cancellation_token.cancelled() => {},
                }
            });
            self.active_tasks.insert(handle, task);
        }
    }
}

/// Resolve `hostname` via the operating system's full name-resolution
/// stack (`getaddrinfo` on Unix, GetAddrInfoEx on Windows). This walks
/// `/etc/nsswitch.conf` on Linux — picking up mDNS via Avahi, /etc/hosts,
/// and DNS — and uses Bonjour for `.local` names on macOS. Pure-DNS
/// resolvers like hickory miss all of those, which is why a Bonjour
/// hostname (e.g. `JKMBP-M4-Max.local`) wouldn't resolve before.
///
/// Port `0` is a placeholder — `lookup_host` requires `host:port` but we
/// only care about the IPs at this stage; the actual port is appended at
/// connection time.
async fn resolve_hostname(hostname: &str) -> io::Result<Vec<IpAddr>> {
    let addrs = lookup_host((hostname, 0)).await?;
    Ok(addrs.map(|sa| sa.ip()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::task::LocalSet;

    async fn next_event(resolver: &mut DnsResolver) -> DnsEvent {
        tokio::time::timeout(Duration::from_secs(10), resolver.event())
            .await
            .expect("timed out waiting for a dns event")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolves_localhost_through_the_resolver_task() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let mut resolver = DnsResolver::new().expect("resolver");
                resolver.resolve(7, "localhost".to_string());

                let mut resolved = false;
                for _ in 0..5 {
                    match next_event(&mut resolver).await {
                        DnsEvent::Resolving(handle) => assert_eq!(handle, 7),
                        DnsEvent::Resolved(handle, hostname, result) => {
                            assert_eq!(handle, 7);
                            assert_eq!(hostname, "localhost");
                            let ips = result.expect("localhost must resolve");
                            assert!(!ips.is_empty());
                            resolved = true;
                            break;
                        }
                    }
                }
                assert!(resolved, "resolver never produced a result");
                resolver.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_resolutions_are_reported_independently() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let mut resolver = DnsResolver::new().expect("resolver");
                resolver.resolve(1, "localhost".to_string());
                resolver.resolve(2, "localhost".to_string());

                let mut handles = Vec::new();
                while handles.len() < 2 {
                    if let DnsEvent::Resolved(handle, _, result) = next_event(&mut resolver).await {
                        result.expect("localhost must resolve");
                        handles.push(handle);
                    }
                }
                handles.sort();
                assert_eq!(handles, vec![1, 2]);
                resolver.terminate().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_new_request_for_the_same_handle_supersedes_the_previous_one() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let mut resolver = DnsResolver::new().expect("resolver");
                resolver.resolve(3, "localhost".to_string());
                resolver.resolve(3, "localhost".to_string());

                let mut resolved = 0;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    match tokio::time::timeout(Duration::from_secs(1), resolver.event()).await {
                        Ok(DnsEvent::Resolved(3, _, result)) => {
                            result.expect("localhost must resolve");
                            resolved += 1;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                assert!(
                    resolved >= 1,
                    "at least the latest request must be resolved"
                );
                resolver.terminate().await;
            })
            .await;
    }
}
