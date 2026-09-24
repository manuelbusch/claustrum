//! Actions across the worker/broker boundary.
//!
//! When the sandbox runs in a confined worker process, the worker cannot
//! start host programs itself (it may not execute anything, and macOS cannot
//! nest Seatbelt profiles). It forwards each invocation to the unconfined
//! broker over a Unix socket instead: [`RemoteActions`] is the worker's
//! [`ActionExecutor`], [`serve`] is the broker loop that validates the request
//! with its own [`ActionHost`] and runs it.
//!
//! The protocol is one JSON object per line. The worker is untrusted (a WASIX
//! escape lands there), so the broker only accepts an action name plus raw
//! inputs, never argv or paths, and caps the line length.

use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};

use super::{
    bind::Refusal,
    host::ActionExecutor,
    run::ActionOutcome,
    spec::{ActionSpec, Confine},
};
use crate::hostcmd::Cancel;

/// Longest request line the broker accepts.
const MAX_REQUEST: u64 = 1024 * 1024;
/// Longest response line the worker accepts (outputs are base64 encoded).
const MAX_RESPONSE: u64 = 256 * 1024 * 1024;
/// How often a waiting worker checks its cancel flag.
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Run {
        id: u64,
        name: String,
        positional: Vec<String>,
        named: BTreeMap<String, String>,
        cwd: String,
    },
    Cancel {
        id: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Response {
    Done { id: u64, outcome: ActionOutcome },
    Refused { id: u64, message: String },
}

fn write_line(stream: &Mutex<UnixStream>, msg: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut s = stream.lock().unwrap_or_else(|p| p.into_inner());
    s.write_all(&line)?;
    s.flush()
}

/// Read one line of at most `max` bytes; `None` at end of stream.
fn read_line(reader: &mut impl BufRead, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let n = reader.by_ref().take(max + 1).read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too long or truncated",
        ));
    }
    line.pop();
    Ok(Some(line))
}

/// The worker's executor: sends each invocation to the broker and waits.
#[derive(Debug)]
pub struct RemoteActions {
    writer: Mutex<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Response>>>>,
    next: AtomicU64,
}

impl RemoteActions {
    /// Take over the worker's end of the socket.
    pub fn new(stream: UnixStream) -> std::io::Result<Arc<Self>> {
        let reader = stream.try_clone()?;
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Response>>>> = Arc::default();
        let map = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("claustrum-actions".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader);
                loop {
                    let line = match read_line(&mut reader, MAX_RESPONSE) {
                        Ok(Some(line)) => line,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!(error = %e, "broker connection failed");
                            break;
                        }
                    };
                    let response: Response = match serde_json::from_slice(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!(error = %e, "invalid message from the broker");
                            continue;
                        }
                    };
                    let id = match &response {
                        Response::Done { id, .. } | Response::Refused { id, .. } => *id,
                    };
                    if let Some(tx) = map.lock().unwrap_or_else(|p| p.into_inner()).remove(&id) {
                        let _ = tx.send(response);
                    }
                }
                // Dropping the senders wakes every waiter with an error.
                map.lock().unwrap_or_else(|p| p.into_inner()).clear();
            })?;
        Ok(Arc::new(Self {
            writer: Mutex::new(stream),
            pending,
            next: AtomicU64::new(1),
        }))
    }
}

impl ActionExecutor for RemoteActions {
    /// The broker only splits off a worker when confinement is active, so
    /// every action that does not opt out is confined.
    fn is_confined(&self, spec: &ActionSpec) -> bool {
        spec.confine == Confine::Os
    }

    fn run(
        &self,
        name: &str,
        positional: &[String],
        named: &BTreeMap<String, String>,
        guest_cwd: &str,
        cancel: &Cancel,
    ) -> Result<ActionOutcome, Refusal> {
        let gone = || Refusal(format!("action `{name}`: the Claustrum broker is gone"));
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        let request = Request::Run {
            id,
            name: name.to_owned(),
            positional: positional.to_vec(),
            named: named.clone(),
            cwd: guest_cwd.to_owned(),
        };
        if write_line(&self.writer, &request).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(gone());
        }
        let mut cancel_sent = false;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Response::Done { outcome, .. }) => return Ok(outcome),
                Ok(Response::Refused { message, .. }) => return Err(Refusal(message)),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(gone()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() && !cancel_sent {
                        cancel_sent = true;
                        let _ = write_line(&self.writer, &Request::Cancel { id });
                    }
                }
            }
        }
    }
}

/// Broker loop: answer the worker's requests with `executor` until the
/// worker closes its end. Blocks; run it on its own thread. Actions still
/// running when the worker goes away are cancelled.
pub fn serve(stream: UnixStream, executor: Arc<dyn ActionExecutor>) -> std::io::Result<()> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let cancels: Arc<Mutex<HashMap<u64, Cancel>>> = Arc::default();
    let mut reader = BufReader::new(stream);
    let result = loop {
        let line = match read_line(&mut reader, MAX_REQUEST) {
            Ok(Some(line)) => line,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let request: Request = match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(e) => {
                // A well-behaved worker never sends this; stop talking to it.
                break Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid request from the worker: {e}"),
                ));
            }
        };
        match request {
            Request::Cancel { id } => {
                if let Some(c) = cancels.lock().unwrap_or_else(|p| p.into_inner()).get(&id) {
                    c.cancel();
                }
            }
            Request::Run {
                id,
                name,
                positional,
                named,
                cwd,
            } => {
                let cancel = Cancel::new();
                cancels
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id, cancel.clone());
                let executor = Arc::clone(&executor);
                let writer = Arc::clone(&writer);
                let cancels = Arc::clone(&cancels);
                std::thread::spawn(move || {
                    let response = match executor.run(&name, &positional, &named, &cwd, &cancel) {
                        Ok(outcome) => Response::Done { id, outcome },
                        Err(r) => Response::Refused {
                            id,
                            message: r.to_string(),
                        },
                    };
                    cancels
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                    if let Err(e) = write_line(&writer, &response) {
                        tracing::warn!(error = %e, "cannot answer the worker");
                    }
                });
            }
        }
    };
    for c in cancels.lock().unwrap_or_else(|p| p.into_inner()).values() {
        c.cancel();
    }
    result
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[derive(Debug)]
    struct Echo;

    impl ActionExecutor for Echo {
        fn is_confined(&self, _: &ActionSpec) -> bool {
            false
        }

        fn run(
            &self,
            name: &str,
            positional: &[String],
            named: &BTreeMap<String, String>,
            guest_cwd: &str,
            cancel: &Cancel,
        ) -> Result<ActionOutcome, Refusal> {
            match name {
                "echo" => Ok(ActionOutcome {
                    stdout: format!("{positional:?} {named:?} {guest_cwd}").into_bytes(),
                    ..Default::default()
                }),
                "wait" => {
                    let started = Instant::now();
                    while !cancel.is_cancelled() {
                        if started.elapsed() > Duration::from_secs(10) {
                            return Err(Refusal("never cancelled".into()));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(ActionOutcome {
                        killed: true,
                        ..Default::default()
                    })
                }
                _ => Err(Refusal(format!("unknown action `{name}`"))),
            }
        }
    }

    fn pair() -> Arc<RemoteActions> {
        let (worker, broker) = UnixStream::pair().unwrap();
        std::thread::spawn(move || serve(broker, Arc::new(Echo)));
        RemoteActions::new(worker).unwrap()
    }

    #[test]
    fn round_trip_and_refusal() {
        let remote = pair();
        let named = [("k".to_owned(), "v".to_owned())].into();
        let out = remote
            .run("echo", &["a".into()], &named, "/workspace", &Cancel::new())
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "[\"a\"] {\"k\": \"v\"} /workspace"
        );
        let err = remote
            .run("nope", &[], &BTreeMap::new(), "/", &Cancel::new())
            .unwrap_err();
        assert!(err.0.contains("unknown action"));
    }

    #[test]
    fn cancel_reaches_the_broker() {
        let remote = pair();
        let cancel = Cancel::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.cancel();
        });
        let out = remote
            .run("wait", &[], &BTreeMap::new(), "/", &cancel)
            .unwrap();
        assert!(out.killed);
    }

    #[test]
    fn broker_rejects_garbage_and_worker_notices() {
        let (mut worker, broker) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || serve(broker, Arc::new(Echo)));
        worker
            .write_all(b"{\"op\":\"exec\",\"argv\":[\"/bin/sh\"]}\n")
            .unwrap();
        assert!(handle.join().unwrap().is_err());

        let (worker, broker) = UnixStream::pair().unwrap();
        drop(broker);
        let remote = RemoteActions::new(worker).unwrap();
        let err = remote
            .run("echo", &[], &BTreeMap::new(), "/", &Cancel::new())
            .unwrap_err();
        assert!(err.0.contains("broker is gone"), "{err}");
    }
}
