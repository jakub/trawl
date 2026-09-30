// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The client that reaches trawld, and the pinned CA behind it (ADR-0048).
//!
//! Under the platform roots the client never changes. Under a pinned CA,
//! `[web] upstream_ca_path` may name a file that does not exist yet, since
//! trawld writes its generated certificate on its first start, and an
//! operator may rotate the file later. So [`Upstream`] reads the file again
//! and swaps in a new client when its contents change:
//!
//! - On a fixed interval, [`CA_REREAD_INTERVAL`] in production, from a task
//!   that `AppState::spawn_upstream_ca_reread` starts.
//! - On a request while no client exists yet. The request takes the client
//!   the read produced, or answers 503 `upstream certificate not
//!   available`.
//!
//! Reads are single-flight, and each runs on tokio's blocking pool, never
//! on a runtime worker: opening, reading and parsing the file and building
//! a client are all synchronous, and a file on a stalled network volume
//! can block for as long as the volume stalls. The open itself never
//! waits, and a path that is not a regular file, such as a FIFO, is a
//! refusal. A request waits for a read at most [`CA_READ_WAIT`], then
//! answers 503; the read goes on, and what it finds is still used. A
//! request that queued behind a read which started after it arrived takes
//! that read's result instead of reading again. Routes that never reach
//! trawld, `/healthz` among them, never wait on the file.
//!
//! A read compares a BLAKE3 digest of the file's bytes with the digest of
//! the bytes behind the client in use, never its modification time.
//! Unchanged bytes do nothing. Bytes that parse become a new client. Bytes
//! that do not parse, a file that disappears, a file that cannot be read,
//! and a file larger than `config::MAX_PIN_FILE_BYTES` (1 MiB) are
//! refusals: the last good client stays in use, and each distinct refusal
//! logs one warning. Before any good load there is no client to keep, so
//! requests stay at 503. Only digests are kept between reads, never a
//! file's bytes.
//!
//! A request takes one client, a cheap clone, when it starts, and uses it
//! until its response body ends. A swap changes what the next request
//! takes and never touches a client already taken.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use reqwest::{Certificate, Client, ClientBuilder};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::config::{UpstreamConnect, UpstreamTls, pinned_roots, read_pin_file};
use crate::error::ProxyError;

/// How often the pinned CA file is read again.
pub const CA_REREAD_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a request waits for a read of the pin file, queueing
/// behind another read included, before it answers 503. A local file reads
/// in well under this; only a stalled volume takes longer, and then a
/// prompt 503 serves the browser better than a hung request.
pub const CA_READ_WAIT: Duration = Duration::from_secs(5);

/// The upstream client for one trust mode.
pub struct Upstream(Trust);

enum Trust {
    System(Client),
    /// Shared with the read in flight on the blocking pool, which outlives
    /// any request that stops waiting for it.
    Pinned(Arc<Pin>),
}

struct Pin {
    /// The pin file, tilde-expanded.
    path: PathBuf,
    connect: Option<UpstreamConnect>,
    /// The client built from the file's last good contents, `None` until
    /// the file first holds a usable certificate. The lock is held only to
    /// clone or replace the client, never across an `.await` or a build.
    client: RwLock<Option<Client>>,
    /// Makes every read of the file single-flight, and owns what the reads
    /// have seen so far. A read holds it from start to end, on the
    /// blocking pool, through an owned guard.
    reread: Arc<Mutex<Observed>>,
    /// How many reads have started. Incremented while holding `reread`,
    /// before the file is opened, so a request that saw a lower count on
    /// arrival and now holds the mutex knows a read that began after it
    /// arrived has finished.
    reads_started: AtomicU64,
    /// Write-locked by a test to stall reads on the blocking pool, as a
    /// stalled volume would.
    #[cfg(test)]
    stall: tokio::sync::RwLock<()>,
}

/// What earlier reads of the pin file found.
struct Observed {
    /// The digest of the contents behind the client in use; `None` while no
    /// client exists.
    loaded: Option<blake3::Hash>,
    /// The refusal last logged, so the same refusal is not logged again.
    /// Cleared by a read that finds the loaded contents or loads new ones.
    refusal: Option<Refusal>,
}

/// Why a read of the pin file produced no client. Two refusals are the
/// same when a re-read would log the same thing: the same kind, and for
/// contents, the same digest.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    Absent,
    Unreadable(std::io::ErrorKind),
    Unusable(blake3::Hash),
}

impl Upstream {
    /// The upstream client for `tls`, dialing `connect` when set.
    ///
    /// A pin whose file did not exist at resolution starts with no client.
    /// Resolution already logged that once.
    ///
    /// # Errors
    /// Returns the `reqwest::Error` if the client cannot be built.
    pub fn new(tls: UpstreamTls, connect: Option<UpstreamConnect>) -> Result<Self, reqwest::Error> {
        match tls {
            UpstreamTls::System => Ok(Self(Trust::System(
                upstream_client(Client::builder(), None, connect.as_ref()).build()?,
            ))),
            UpstreamTls::PinnedCa { path, roots } => {
                let (client, observed) = match roots {
                    Some(roots) => (
                        Some(
                            upstream_client(
                                Client::builder(),
                                Some(roots.certificates()),
                                connect.as_ref(),
                            )
                            .build()?,
                        ),
                        Observed {
                            loaded: Some(roots.digest()),
                            refusal: None,
                        },
                    ),
                    None => (
                        None,
                        Observed {
                            loaded: None,
                            refusal: Some(Refusal::Absent),
                        },
                    ),
                };
                Ok(Self(Trust::Pinned(Arc::new(Pin {
                    path,
                    connect,
                    client: RwLock::new(client),
                    reread: Arc::new(Mutex::new(observed)),
                    reads_started: AtomicU64::new(0),
                    #[cfg(test)]
                    stall: tokio::sync::RwLock::new(()),
                }))))
            }
        }
    }

    /// Whether the client can change, so a re-read task has work to do.
    pub fn is_pinned(&self) -> bool {
        matches!(self.0, Trust::Pinned(_))
    }

    /// The client for one request.
    ///
    /// While a pin has no client, this reads the file once, single-flight
    /// with every other read, and takes the client that read produced. A
    /// read that started after this request arrived and finished while it
    /// queued serves instead of a new one.
    ///
    /// # Errors
    /// [`ProxyError::UpstreamCertificateUnavailable`] when the pin file has
    /// never held a usable certificate, or no read finished within
    /// [`CA_READ_WAIT`].
    pub async fn client(&self) -> Result<Client, ProxyError> {
        let pin = match &self.0 {
            Trust::System(client) => return Ok(client.clone()),
            Trust::Pinned(pin) => pin,
        };
        if let Some(client) = pin.current() {
            return Ok(client);
        }
        let arrived = pin.reads_started.load(Ordering::Acquire);
        let read = async {
            let observed = Arc::clone(&pin.reread).lock_owned().await;
            // A client may have been loaded while this request queued. A
            // read that began after it arrived has finished, too: its
            // result, a client or none, answers this request as well.
            if pin.current().is_some() || pin.reads_started.load(Ordering::Acquire) != arrived {
                return;
            }
            Pin::observe(Arc::clone(pin), observed).await;
        };
        // On timeout the read in flight, this request's or another's, goes
        // on and installs what it finds; only the wait ends.
        let _ = tokio::time::timeout(CA_READ_WAIT, read).await;
        pin.current()
            .ok_or(ProxyError::UpstreamCertificateUnavailable)
    }

    /// Read the pin file again, and swap in a new client if its contents
    /// changed and parse. Nothing to do under the platform roots.
    pub async fn reread(&self) {
        if let Trust::Pinned(pin) = &self.0 {
            let observed = Arc::clone(&pin.reread).lock_owned().await;
            Pin::observe(Arc::clone(pin), observed).await;
        }
    }
}

impl Pin {
    fn current(&self) -> Option<Client> {
        self.client
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Read the file once on the blocking pool, holding `observed`, the
    /// re-read mutex, until the read has acted on what it found.
    ///
    /// The read cannot be cancelled: dropping this future stops only the
    /// wait for it.
    async fn observe(pin: Arc<Self>, mut observed: OwnedMutexGuard<Observed>) {
        pin.reads_started.fetch_add(1, Ordering::AcqRel);
        // The read logs as if it ran here: to this thread's subscriber,
        // inside the span of the request or task that started it.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let span = tracing::Span::current();
        let read = tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                span.in_scope(|| {
                    #[cfg(test)]
                    drop(pin.stall.blocking_read());
                    pin.observe_blocking(&mut observed);
                });
            });
        });
        if let Err(e) = read.await
            && e.is_panic()
        {
            std::panic::resume_unwind(e.into_panic());
        }
    }

    /// Read the file once and act on what it holds. Blocks; the caller
    /// holds the re-read mutex, whose contents `observed` is.
    fn observe_blocking(&self, observed: &mut Observed) {
        let read = read_pin_file(&self.path).map(|pem| {
            let digest = blake3::hash(&pem);
            (pem, digest)
        });
        let (refusal, reason) = match read {
            Ok((_, digest)) if observed.loaded == Some(digest) => {
                observed.refusal = None;
                return;
            }
            Ok((pem, digest)) => match self.build(&pem) {
                Ok(client) => {
                    let first = observed.loaded.is_none();
                    *self.client.write().unwrap_or_else(PoisonError::into_inner) = Some(client);
                    observed.loaded = Some(digest);
                    observed.refusal = None;
                    if first {
                        tracing::info!(
                            event_type = "upstream_ca_loaded",
                            path = %self.path.display(),
                            "loaded the pinned upstream CA file; trawl-web can reach trawld"
                        );
                    } else {
                        tracing::info!(
                            event_type = "upstream_ca_reloaded",
                            path = %self.path.display(),
                            "the pinned upstream CA file changed; new requests trust its certificates"
                        );
                    }
                    return;
                }
                Err(reason) => (Refusal::Unusable(digest), reason.to_owned()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Refusal::Absent, "the file does not exist".to_owned())
            }
            Err(e) => (Refusal::Unreadable(e.kind()), e.to_string()),
        };
        if observed.refusal.as_ref() == Some(&refusal) {
            return;
        }
        observed.refusal = Some(refusal);
        if observed.loaded.is_some() {
            tracing::warn!(
                event_type = "upstream_ca_refused",
                path = %self.path.display(),
                reason = %reason,
                "refused the pinned upstream CA file; trawl-web keeps trusting the certificates it last loaded"
            );
        } else {
            tracing::warn!(
                event_type = "upstream_ca_refused",
                path = %self.path.display(),
                reason = %reason,
                "refused the pinned upstream CA file; trawl-web cannot reach trawld until it holds a usable certificate"
            );
        }
    }

    /// A client trusting only the certificates in `pem`.
    fn build(&self, pem: &[u8]) -> Result<Client, &'static str> {
        let roots = pinned_roots(pem)?;
        upstream_client(
            Client::builder(),
            Some(roots.certificates()),
            self.connect.as_ref(),
        )
        .build()
        .map_err(|_| "no upstream client can be built from its certificates")
    }
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Trust::System(_) => f.write_str("Upstream::System"),
            Trust::Pinned(pin) => f
                .debug_struct("Upstream::Pinned")
                .field("path", &pin.path)
                .field("connect", &pin.connect)
                .field("loaded", &pin.current().is_some())
                .finish(),
        }
    }
}

/// Set the upstream trust and dialing rules on `builder`.
///
/// In every mode the client:
///
/// - Speaks https only. Resolution already refuses a plain-http
///   `upstream_url`; this also refuses any other cleartext hop.
/// - Follows no redirect. trawld never sends one, and a followed 3xx would
///   carry the signed-in user's key to whatever host the `Location` names
///   and the trust mode accepts: under the platform roots, any host with a
///   public certificate.
/// - Ignores proxy settings, such as `HTTPS_PROXY`. trawl-web always dials
///   trawld directly. A proxy would also resolve the host name on its own
///   side, past any connect address.
///
/// With a connect address (`[web] upstream_connect_addr`), the client dials
/// that address for the URL's host and never asks the resolver. TLS still
/// verifies the host name from the URL.
///
/// `pinned` certificates replace the platform roots, with host-name
/// verification left on. `None` trusts the platform roots.
///
/// Takes a builder so a caller can add settings the rules leave alone. A
/// test passes one with its own resolver or proxy and sees what the rules
/// override; the doctor passes one with its timeouts, no retries and no
/// connection pool, and so probes trawld through the same rules the
/// proxy's requests use.
pub(crate) fn upstream_client(
    builder: ClientBuilder,
    pinned: Option<&[Certificate]>,
    connect: Option<&UpstreamConnect>,
) -> ClientBuilder {
    let builder = builder
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .no_proxy();
    let builder = match connect {
        Some(connect) => builder.resolve(&connect.host, connect.addr),
        None => builder,
    };
    match pinned {
        None => builder,
        Some(roots) => builder.tls_certs_only(roots.iter().cloned()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use reqwest::dns::{Name, Resolve, Resolving};
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::MAX_PIN_FILE_BYTES;
    use crate::test_support::{TestCa, TlsUpstream, captured_logs};

    /// The platform roots and a pin to a fresh CA: the rules under test do
    /// not depend on which roots are trusted.
    fn every_mode() -> [Option<Vec<Certificate>>; 2] {
        let ca = TestCa::generate();
        let roots = pinned_roots(ca.pem().as_bytes()).expect("test CA parses");
        [None, Some(roots.certificates().to_vec())]
    }

    fn mode_name(pinned: Option<&[Certificate]>) -> &'static str {
        if pinned.is_some() { "pinned" } else { "system" }
    }

    /// Send one request to `url` with `client` and report whether
    /// `listener` saw a connection. The TLS handshake then fails, since
    /// the listener speaks no TLS; only the dial matters here.
    async fn dials(client: &Client, url: String, listener: &TcpListener) -> bool {
        let request = tokio::spawn(client.get(url).send());
        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .is_ok_and(|accept| accept.is_ok());
        request.abort();
        accepted
    }

    /// A host resolver that answers nothing and counts how often it is
    /// asked, standing in for one that would name another machine.
    struct RefusingResolver(Arc<AtomicUsize>);

    impl Resolve for RefusingResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(
                Err("the host resolver was asked".into()),
            ))
        }
    }

    #[tokio::test]
    async fn a_connect_addr_is_dialed_without_asking_the_resolver() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for pinned in every_mode() {
            let pinned = pinned.as_deref();
            let mode = mode_name(pinned);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("https://trawl.test:{}/api/v1/whoami", addr.port());

            // Control: without a connect address the name goes to the
            // resolver, which refuses, so the injected resolver is in use.
            let asked = Arc::new(AtomicUsize::new(0));
            let base = Client::builder().dns_resolver(Arc::new(RefusingResolver(asked.clone())));
            let client = upstream_client(base, pinned, None).build().unwrap();
            let err = client.get(&url).send().await.expect_err("nothing resolves");
            assert!(err.is_connect(), "{mode}: {err:?}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{mode}");

            let asked = Arc::new(AtomicUsize::new(0));
            let base = Client::builder().dns_resolver(Arc::new(RefusingResolver(asked.clone())));
            let connect = UpstreamConnect {
                host: "trawl.test".to_owned(),
                addr,
            };
            let client = upstream_client(base, pinned, Some(&connect))
                .build()
                .unwrap();
            assert!(
                dials(&client, url, &listener).await,
                "{mode}: the client did not dial the connect address"
            );
            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "{mode}: the client asked the resolver for a name it has an address for"
            );
        }
    }

    #[tokio::test]
    async fn every_mode_ignores_a_configured_proxy() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for pinned in every_mode() {
            let pinned = pinned.as_deref();
            let mode = mode_name(pinned);
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = Client::builder().proxy(
                reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap(),
            );
            let client = upstream_client(base, pinned, None).build().unwrap();

            let url = format!(
                "https://127.0.0.1:{}/api/v1/whoami",
                upstream.local_addr().unwrap().port()
            );
            assert!(
                dials(&client, url, &upstream).await,
                "{mode}: the client did not dial the upstream directly"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(200), proxy.accept())
                    .await
                    .is_err(),
                "{mode}: the client went through the proxy"
            );
        }
    }

    #[tokio::test]
    async fn every_mode_refuses_a_plain_http_request() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for pinned in every_mode() {
            let pinned = pinned.as_deref();
            let mode = mode_name(pinned);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = upstream_client(Client::builder(), pinned, None)
                .build()
                .unwrap();
            let url = format!(
                "http://127.0.0.1:{}/api/v1/whoami",
                listener.local_addr().unwrap().port()
            );
            // Bounded: a client that did dial would wait forever on the
            // silent listener, and the test must fail rather than hang.
            let err = tokio::time::timeout(Duration::from_secs(5), client.get(&url).send())
                .await
                .unwrap_or_else(|_| panic!("{mode}: the client dialed a plain-http upstream"))
                .expect_err("a cleartext request must refuse");
            assert!(err.is_builder(), "{mode}: {err:?}");
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "{mode}: the client dialed a plain-http upstream"
            );
        }
    }

    /// A pin resolved from `path`, as startup resolves it.
    fn pinned(path: &Path) -> Upstream {
        let tls = crate::config::resolve_upstream_tls(Some(path)).expect("the pin resolves");
        Upstream::new(tls, None).expect("the client builds")
    }

    /// Whether `client` completes a request to `upstream`: its certificate
    /// chain and name are trusted. Bounded, so a hang fails the test.
    async fn trusts(client: &Client, upstream: &TlsUpstream) -> bool {
        tokio::time::timeout(
            Duration::from_secs(10),
            client.get(format!("{}/", upstream.url())).send(),
        )
        .await
        .expect("the request ended")
        .is_ok()
    }

    /// A pending pin answers 503 without dialing anything, and each
    /// request reads the file again, so the first one after it appears
    /// gets a client.
    #[tokio::test]
    async fn a_pending_pin_is_unavailable_until_its_file_appears() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tls").join("cert.pem");
        let upstream = pinned(&path);
        assert!(matches!(
            upstream.client().await,
            Err(ProxyError::UpstreamCertificateUnavailable)
        ));

        let ca = TestCa::generate();
        let front = TlsUpstream::issued_by(&ca).await;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, ca.pem()).unwrap();
        let client = upstream.client().await.expect("the file is there now");
        assert!(trusts(&client, &front).await);
    }

    /// A swap changes the client the next request takes, never one a
    /// request already holds: a request keeps the trust it started with
    /// until it ends.
    #[tokio::test]
    async fn a_client_taken_before_a_swap_keeps_its_roots() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (ca_a, ca_b) = (TestCa::generate(), TestCa::generate());
        let front_a = TlsUpstream::issued_by(&ca_a).await;
        let front_b = TlsUpstream::issued_by(&ca_b).await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, ca_a.pem()).unwrap();
        let upstream = pinned(&path);
        let before = upstream.client().await.unwrap();

        std::fs::write(&path, ca_b.pem()).unwrap();
        upstream.reread().await;
        let after = upstream.client().await.unwrap();

        assert!(trusts(&before, &front_a).await, "the held client changed");
        assert!(!trusts(&before, &front_b).await, "the held client changed");
        assert!(trusts(&after, &front_b).await, "the swap did not happen");
        assert!(!trusts(&after, &front_a).await, "the swap kept the old CA");
    }

    fn pin_of(upstream: &Upstream) -> &Arc<Pin> {
        match &upstream.0 {
            Trust::Pinned(pin) => pin,
            Trust::System(_) => panic!("not a pin"),
        }
    }

    /// A read that outlasts [`CA_READ_WAIT`]: the request answers 503 when
    /// its wait ends, and the read goes on to install the client it builds.
    #[tokio::test]
    async fn a_request_stops_waiting_for_a_stalled_read() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let upstream = pinned(&path);
        let pin = Arc::clone(pin_of(&upstream));
        std::fs::write(&path, TestCa::generate().pem()).unwrap();

        let stall = pin.stall.write().await;
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(CA_READ_WAIT * 2, upstream.client())
            .await
            .expect("the request kept waiting past its bound");
        let waited = started.elapsed();
        assert!(
            matches!(outcome, Err(ProxyError::UpstreamCertificateUnavailable)),
            "{outcome:?}"
        );
        assert!(
            waited >= CA_READ_WAIT,
            "the request gave up before its bound: {waited:?}"
        );
        assert!(pin.current().is_none());

        drop(stall);
        // The read holds the re-read mutex until it has acted, so once the
        // mutex is free the read has finished.
        drop(
            tokio::time::timeout(Duration::from_secs(10), pin.reread.lock())
                .await
                .expect("the stalled read finished"),
        );
        assert!(
            pin.current().is_some(),
            "the stalled read's client was not installed"
        );
        assert_eq!(pin.reads_started.load(Ordering::SeqCst), 1);
    }

    /// Requests that queue behind a read which started before they arrived
    /// read the file once more between them: the first to get the mutex
    /// reads, and the rest take that read's result.
    #[tokio::test]
    async fn queued_requests_share_one_read() {
        use futures::FutureExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let upstream = pinned(&path);
        let pin = Arc::clone(pin_of(&upstream));

        let stall = pin.stall.write().await;
        let mut first = Box::pin(upstream.client());
        assert!((&mut first).now_or_never().is_none(), "the read stalls");
        assert_eq!(pin.reads_started.load(Ordering::SeqCst), 1);
        let mut queued: Vec<_> = (0..3).map(|_| Box::pin(upstream.client())).collect();
        for request in &mut queued {
            assert!(request.now_or_never().is_none(), "the request queues");
        }
        drop(stall);

        assert!(first.await.is_err());
        for request in queued {
            assert!(request.await.is_err());
        }
        assert_eq!(
            pin.reads_started.load(Ordering::SeqCst),
            2,
            "each queued request read the file itself"
        );
    }

    /// Each distinct refusal logs one warning, however often a re-read
    /// meets it.
    #[test]
    fn a_refusal_warns_once_until_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, TestCa::generate().pem()).unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let upstream = pinned(&path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let reread_with = |contents: &str| {
            std::fs::write(&path, contents).unwrap();
            runtime.block_on(upstream.reread());
        };

        let ((), lines) = captured_logs(|| {
            reread_with("not a certificate\n");
            reread_with("not a certificate\n");
        });
        let warnings: Vec<_> = lines.iter().filter(|l| l.starts_with("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{lines:?}");
        assert!(warnings[0].contains("upstream_ca_refused"), "{lines:?}");
        assert!(warnings[0].contains("last loaded"), "{lines:?}");

        let ((), lines) = captured_logs(|| {
            reread_with("still not a certificate\n");
            reread_with("still not a certificate\n");
        });
        let warnings: Vec<_> = lines.iter().filter(|l| l.starts_with("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{lines:?}");

        // The file disappearing is another refusal, logged once too.
        let ((), lines) = captured_logs(|| {
            std::fs::remove_file(&path).unwrap();
            runtime.block_on(upstream.reread());
            runtime.block_on(upstream.reread());
        });
        let warnings: Vec<_> = lines.iter().filter(|l| l.starts_with("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{lines:?}");
        assert!(warnings[0].contains("does not exist"), "{lines:?}");

        // Throughout, the last good client stays in use.
        assert!(runtime.block_on(upstream.client()).is_ok());
    }

    /// A pin file that grows past [`MAX_PIN_FILE_BYTES`] after startup is a
    /// refusal like any other: one warning, and the last good client stays.
    /// The file is sparse, so the test allocates nothing.
    #[test]
    fn an_oversized_pin_file_is_refused_on_reread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, TestCa::generate().pem()).unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let upstream = pinned(&path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let ((), lines) = captured_logs(|| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(MAX_PIN_FILE_BYTES + 1)
                .unwrap();
            runtime.block_on(upstream.reread());
            runtime.block_on(upstream.reread());
        });
        let warnings: Vec<_> = lines.iter().filter(|l| l.starts_with("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{lines:?}");
        assert!(warnings[0].contains("larger than 1 MiB"), "{lines:?}");
        assert!(warnings[0].contains("last loaded"), "{lines:?}");
        assert!(runtime.block_on(upstream.client()).is_ok());
        assert_eq!(
            pin_of(&upstream)
                .reread
                .try_lock()
                .expect("no read in flight")
                .refusal,
            Some(Refusal::Unreadable(std::io::ErrorKind::FileTooLarge))
        );
    }

    /// A pin still waiting for its first good file: its absence was logged
    /// at resolution and is not logged again, while unusable contents are
    /// logged once each.
    #[test]
    fn a_pending_pin_warns_once_per_unusable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (upstream, _) = captured_logs(|| pinned(&path));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let ((), lines) = captured_logs(|| {
            runtime.block_on(upstream.reread());
            assert!(runtime.block_on(upstream.client()).is_err());
        });
        assert!(lines.is_empty(), "{lines:?}");

        let ((), lines) = captured_logs(|| {
            std::fs::write(&path, "garbage\n").unwrap();
            runtime.block_on(upstream.reread());
            assert!(runtime.block_on(upstream.client()).is_err());
        });
        let warnings: Vec<_> = lines.iter().filter(|l| l.starts_with("WARN")).collect();
        assert_eq!(warnings.len(), 1, "{lines:?}");
        assert!(warnings[0].contains("cannot reach trawld"), "{lines:?}");

        let ca = TestCa::generate();
        let ((), lines) = captured_logs(|| {
            std::fs::write(&path, ca.pem()).unwrap();
            runtime.block_on(upstream.reread());
        });
        assert!(
            lines.len() == 1
                && lines[0].starts_with("INFO")
                && lines[0].contains("upstream_ca_loaded"),
            "{lines:?}"
        );
        assert!(runtime.block_on(upstream.client()).is_ok());
    }
}
