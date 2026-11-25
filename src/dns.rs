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
    resolve_with(hostname, lookup).await
}

async fn lookup(hostname: String) -> io::Result<Vec<IpAddr>> {
    let addrs = lookup_host((hostname.as_str(), 0)).await?;
    Ok(addrs.map(|sa| sa.ip()).collect())
}

/// Resolve `hostname`, and if a bare name (no dot) finds nothing, try it
/// again as `<name>.local`.
///
/// Linux only asks the local network (mDNS) for names that end in `.local`;
/// a bare Windows/macOS computer name such as `DESKTOP-ABC` then fails,
/// although the machine answers to `DESKTOP-ABC.local`. Without the second
/// try a peer whose DHCP address changed could no longer be found by name.
async fn resolve_with<F, Fut>(hostname: &str, lookup: F) -> io::Result<Vec<IpAddr>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = io::Result<Vec<IpAddr>>>,
{
    let first = lookup(hostname.to_string()).await;
    if matches!(&first, Ok(ips) if !ips.is_empty()) {
        return first;
    }
    let Some(local_name) = mdns_fallback(hostname) else {
        return first;
    };
    match lookup(local_name.clone()).await {
        Ok(ips) if !ips.is_empty() => {
            log::info!("resolved {hostname} as {local_name}: {ips:?}");
            Ok(ips)
        }
        // report the error of the name the user typed, not of our guess
        _ => first,
    }
}

/// The `.local` name to try for a bare computer name, or `None` when the
/// name already has a domain (or is an address, or `localhost`).
fn mdns_fallback(hostname: &str) -> Option<String> {
    let name = hostname.trim();
    if name.is_empty()
        || name.contains('.')
        || name.parse::<IpAddr>().is_ok()
        || name.eq_ignore_ascii_case("localhost")
    {
        return None;
    }
    Some(format!("{name}.local"))
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

    use std::{cell::RefCell, net::Ipv4Addr};

    fn not_found() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "no such host")
    }

    /// A fake resolver that knows only `known`, and records what it was
    /// asked.
    fn fake<'a>(
        known: &'static [(&'static str, [u8; 4])],
        asked: &'a RefCell<Vec<String>>,
    ) -> impl Fn(String) -> std::future::Ready<io::Result<Vec<IpAddr>>> + 'a {
        move |name: String| {
            asked.borrow_mut().push(name.clone());
            std::future::ready(
                known
                    .iter()
                    .find(|(host, _)| *host == name)
                    .map(|(_, ip)| vec![IpAddr::V4(Ipv4Addr::from(*ip))])
                    .ok_or_else(not_found),
            )
        }
    }

    #[test]
    fn only_bare_computer_names_get_a_local_fallback() {
        assert_eq!(
            mdns_fallback("DESKTOP-GABRIEL").as_deref(),
            Some("DESKTOP-GABRIEL.local")
        );
        assert_eq!(mdns_fallback(" sala ").as_deref(), Some("sala.local"));
        for no_fallback in [
            "",
            "   ",
            "DESKTOP-GABRIEL.local",
            "pc.example.com",
            "pc.",
            "192.168.1.20",
            "::1",
            "localhost",
            "LocalHost",
        ] {
            assert_eq!(mdns_fallback(no_fallback), None, "{no_fallback:?}");
        }
    }

    /// The case from the user's network: the bare name fails, the `.local`
    /// one answers with the machine's address.
    #[tokio::test(flavor = "current_thread")]
    async fn a_bare_name_that_fails_is_found_as_dot_local() {
        let asked = RefCell::new(Vec::new());
        let ips = resolve_with(
            "DESKTOP-GABRIEL",
            fake(&[("DESKTOP-GABRIEL.local", [192, 168, 166, 156])], &asked),
        )
        .await
        .expect("found through .local");
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(192, 168, 166, 156))]);
        assert_eq!(
            *asked.borrow(),
            ["DESKTOP-GABRIEL", "DESKTOP-GABRIEL.local"]
        );
    }

    /// A name that resolves as typed is used as is, without a second query.
    #[tokio::test(flavor = "current_thread")]
    async fn a_name_that_resolves_is_not_asked_again() {
        let asked = RefCell::new(Vec::new());
        let ips = resolve_with(
            "sala",
            fake(
                &[("sala", [10, 0, 0, 1]), ("sala.local", [10, 0, 0, 2])],
                &asked,
            ),
        )
        .await
        .expect("resolves");
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]);
        assert_eq!(*asked.borrow(), ["sala"]);
    }

    /// A name with a domain is never rewritten, and when neither form
    /// resolves the error is the one of the name that was typed.
    #[tokio::test(flavor = "current_thread")]
    async fn failures_stay_failures_and_domains_are_left_alone() {
        let asked = RefCell::new(Vec::new());
        assert!(
            resolve_with("pc.example.com", fake(&[], &asked))
                .await
                .is_err()
        );
        assert_eq!(*asked.borrow(), ["pc.example.com"]);

        asked.borrow_mut().clear();
        let error = resolve_with("ninguem", fake(&[], &asked))
            .await
            .expect_err("nothing resolves");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(*asked.borrow(), ["ninguem", "ninguem.local"]);
    }

    /// An empty answer counts as "not found" for the fallback.
    #[tokio::test(flavor = "current_thread")]
    async fn an_empty_answer_also_tries_dot_local() {
        let ips = resolve_with("vazio", |name: String| {
            std::future::ready(Ok(if name == "vazio.local" {
                vec![IpAddr::V4(Ipv4Addr::new(10, 1, 1, 1))]
            } else {
                Vec::new()
            }))
        })
        .await
        .expect("resolves");
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(10, 1, 1, 1))]);
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
