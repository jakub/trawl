// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A TCP proxy in front of a test's Postgres that can hold back one
//! session's answers.
//!
//! Armed with a pattern, the proxy watches what clients send. The first
//! session whose message contains the pattern has every answer Postgres
//! sends it from then on held back, until the test releases them. What the
//! client sends still reaches Postgres at once, so Postgres has run the
//! statement while the client waits for its answer: a client that awaits
//! the answer parks, and one that gives up leaves Postgres in whatever
//! state the statement put the session. Other sessions pass untouched.
//!
//! Shared by `#[path]` with the trawl-server tests, which hold the
//! answers of its investigation writes the same way.

#![allow(dead_code)] // each including test uses part of it

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sqlx::postgres::{PgConnectOptions, PgSslMode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};

/// How many sent bytes a session keeps to find a pattern split across two
/// reads: the longest pattern a test arms, less one, fits.
const TAIL: usize = 63;

pub struct Proxy {
    pub port: u16,
    shared: Arc<Shared>,
}

struct Shared {
    armed: Mutex<Option<&'static [u8]>>,
    /// One gate per session: `true` while its answers are held back.
    gates: Mutex<Vec<Arc<watch::Sender<bool>>>>,
    holding: Notify,
    closed: Notify,
    let_through: AtomicUsize,
}

impl Proxy {
    pub async fn start(upstream: &PgConnectOptions) -> Self {
        let upstream = format!("{}:{}", upstream.get_host(), upstream.get_port());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            armed: Mutex::new(None),
            gates: Mutex::new(Vec::new()),
            holding: Notify::new(),
            closed: Notify::new(),
            let_through: AtomicUsize::new(0),
        });
        let accepting = Arc::clone(&shared);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = TcpStream::connect(&upstream).await.unwrap();
                let gate = Arc::new(watch::channel(false).0);
                accepting.gates.lock().unwrap().push(Arc::clone(&gate));
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                tokio::spawn(sent(
                    client_read,
                    server_write,
                    Arc::clone(&accepting),
                    Arc::clone(&gate),
                ));
                tokio::spawn(answered(
                    server_read,
                    client_write,
                    Arc::clone(&accepting),
                    gate,
                ));
            }
        });
        Self { port, shared }
    }

    pub fn options(&self, base: &PgConnectOptions) -> PgConnectOptions {
        base.clone()
            .host("127.0.0.1")
            .port(self.port)
            .ssl_mode(PgSslMode::Disable)
    }

    pub fn hold_next(&self, pattern: &'static [u8]) {
        *self.shared.armed.lock().unwrap() = Some(pattern);
    }

    pub async fn holding(&self) {
        self.shared.holding.notified().await;
    }

    pub async fn closed(&self) {
        self.shared.closed.notified().await;
    }

    pub fn release(&self) {
        for gate in self.shared.gates.lock().unwrap().iter() {
            gate.send_replace(false);
        }
    }

    /// How many held answers have reached their clients since the proxy
    /// started. A test whose runtime thread it blocks on purpose cannot
    /// await, so it reads this instead.
    pub fn let_through(&self) -> usize {
        self.shared.let_through.load(Ordering::SeqCst)
    }
}

/// Client to Postgres: forward everything, closing the session's gate
/// before the armed pattern goes out, so its answer cannot slip through.
async fn sent(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    shared: Arc<Shared>,
    gate: Arc<watch::Sender<bool>>,
) {
    let mut buf = vec![0_u8; 8192];
    let mut tail: Vec<u8> = Vec::new();
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                if *gate.borrow() {
                    shared.closed.notify_one();
                }
                return;
            }
            Ok(n) => n,
        };
        // What came before this read is kept only to find a pattern split
        // across two reads; a match must end in the new bytes.
        let before = tail.len();
        tail.extend_from_slice(&buf[..n]);
        {
            let mut armed = shared.armed.lock().unwrap();
            if let Some(pattern) = *armed
                && tail[before.saturating_sub(pattern.len() - 1)..]
                    .windows(pattern.len())
                    .any(|w| w == pattern)
            {
                *armed = None;
                gate.send_replace(true);
            }
        }
        let keep = tail.len().saturating_sub(TAIL);
        tail.drain(..keep);
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

async fn answered(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    shared: Arc<Shared>,
    gate: Arc<watch::Sender<bool>>,
) {
    let mut buf = vec![0_u8; 8192];
    let mut held = gate.subscribe();
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let was_held = *held.borrow_and_update();
        if was_held {
            shared.holding.notify_one();
            if held.wait_for(|held| !held).await.is_err() {
                return;
            }
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
        if was_held {
            shared.let_through.fetch_add(1, Ordering::SeqCst);
        }
    }
}
