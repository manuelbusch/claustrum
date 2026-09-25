//! Actions and plan files across the worker/broker boundary.
//!
//! When the sandbox runs in a confined worker process, the worker cannot
//! start host programs itself (it may not execute anything, and macOS cannot
//! nest Seatbelt profiles), nor reach Claude Code's plan directory. It
//! forwards each invocation to the unconfined broker over a Unix socket
//! instead: [`BrokerClient`] is the worker's [`ActionExecutor`] and
//! [`PlanStore`], [`serve`] is the broker loop that validates the request
//! with its own [`ActionHost`] or [`HostPlans`](crate::plans::HostPlans)
//! and carries it out.
//!
//! The protocol is one JSON object per line. The worker is untrusted (a WASIX
//! escape lands there), so the broker only accepts an action name plus raw
//! inputs, or a plan file name plus text, never argv or paths, and caps the
//! line length.

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
use crate::{
    hostcmd::Cancel,
    native::{EditOutput, WriteOutput},
    plans::PlanStore,
};

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
    WritePlan {
        id: u64,
        name: String,
        content: String,
    },
    EditPlan {
        id: u64,
        name: String,
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Response {
    Done { id: u64, outcome: ActionOutcome },
    Refused { id: u64, message: String },
    PlanWritten { id: u64, out: WriteOutput },
    PlanEdited { id: u64, out: EditOutput },
}

impl Response {
    fn id(&self) -> u64 {
        match self {
            Response::Done { id, .. }
            | Response::Refused { id, .. }
            | Response::PlanWritten { id, .. }
            | Response::PlanEdited { id, .. } => *id,
        }
    }
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

/// The worker's side of the socket: sends each action invocation or plan
/// write to the broker and waits for the answer.
#[derive(Debug)]
pub struct BrokerClient {
    writer: Mutex<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Response>>>>,
    next: AtomicU64,
}

impl BrokerClient {
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
                    let id = response.id();
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

impl BrokerClient {
    /// Send the request built by `make` (with a fresh id) and wait for its
    /// answer. With `cancel`, a cancellation is forwarded to the broker.
    fn call(
        &self,
        what: &str,
        make: impl FnOnce(u64) -> Request,
        cancel: Option<&Cancel>,
    ) -> Result<Response, Refusal> {
        let gone = || Refusal(format!("{what}: the Claustrum broker is gone"));
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        if write_line(&self.writer, &make(id)).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(gone());
        }
        let mut cancel_sent = false;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Response::Refused { message, .. }) => return Err(Refusal(message)),
                Ok(response) => return Ok(response),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(gone()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(cancel) = cancel
                        && cancel.is_cancelled()
                        && !cancel_sent
                    {
                        cancel_sent = true;
                        let _ = write_line(&self.writer, &Request::Cancel { id });
                    }
                }
            }
        }
    }
}

fn unexpected(what: &str) -> Refusal {
    Refusal(format!(
        "{what}: unexpected answer from the Claustrum broker"
    ))
}

impl ActionExecutor for BrokerClient {
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
        let what = format!("action `{name}`");
        let request = |id| Request::Run {
            id,
            name: name.to_owned(),
            positional: positional.to_vec(),
            named: named.clone(),
            cwd: guest_cwd.to_owned(),
        };
        match self.call(&what, request, Some(cancel))? {
            Response::Done { outcome, .. } => Ok(outcome),
            _ => Err(unexpected(&what)),
        }
    }
}

impl PlanStore for BrokerClient {
    fn write(&self, name: &str, content: &str) -> Result<WriteOutput, String> {
        let request = |id| Request::WritePlan {
            id,
            name: name.to_owned(),
            content: content.to_owned(),
        };
        match self.call("plan", request, None) {
            Ok(Response::PlanWritten { out, .. }) => Ok(out),
            Ok(_) => Err(unexpected("plan").0),
            Err(r) => Err(r.0),
        }
    }

    fn edit(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<EditOutput, String> {
        let request = |id| Request::EditPlan {
            id,
            name: name.to_owned(),
            old_string: old_string.to_owned(),
            new_string: new_string.to_owned(),
            replace_all,
        };
        match self.call("plan", request, None) {
            Ok(Response::PlanEdited { out, .. }) => Ok(out),
            Ok(_) => Err(unexpected("plan").0),
            Err(r) => Err(r.0),
        }
    }
}

/// Broker loop: answer the worker's requests with `executor` (actions) and
/// `plans` until the worker closes its end. Requests for a missing one are
/// refused. Blocks; run it on its own thread. Actions still running when the
/// worker goes away are cancelled.
pub fn serve(
    stream: UnixStream,
    executor: Option<Arc<dyn ActionExecutor>>,
    plans: Option<Arc<dyn PlanStore>>,
) -> std::io::Result<()> {
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
            Request::WritePlan { id, name, content } => {
                let response = match &plans {
                    Some(plans) => match plans.write(&name, &content) {
                        Ok(out) => Response::PlanWritten { id, out },
                        Err(message) => Response::Refused { id, message },
                    },
                    None => no_plans(id),
                };
                answer(&writer, &response);
            }
            Request::EditPlan {
                id,
                name,
                old_string,
                new_string,
                replace_all,
            } => {
                let response = match &plans {
                    Some(plans) => match plans.edit(&name, &old_string, &new_string, replace_all) {
                        Ok(out) => Response::PlanEdited { id, out },
                        Err(message) => Response::Refused { id, message },
                    },
                    None => no_plans(id),
                };
                answer(&writer, &response);
            }
            Request::Run { id, name, .. } if executor.is_none() => {
                answer(
                    &writer,
                    &Response::Refused {
                        id,
                        message: format!("action `{name}`: no host actions are configured"),
                    },
                );
            }
            Request::Run {
                id,
                name,
                positional,
                named,
                cwd,
            } => {
                let executor = executor.clone().expect("checked above");
                let cancel = Cancel::new();
                cancels
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id, cancel.clone());
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
                    answer(&writer, &response);
                });
            }
        }
    };
    for c in cancels.lock().unwrap_or_else(|p| p.into_inner()).values() {
        c.cancel();
    }
    result
}

fn answer(writer: &Mutex<UnixStream>, response: &Response) {
    if let Err(e) = write_line(writer, response) {
        tracing::warn!(error = %e, "cannot answer the worker");
    }
}

fn no_plans(id: u64) -> Response {
    Response::Refused {
        id,
        message: "plan files are disabled ([claude] plans = false)".into(),
    }
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

    fn pair() -> Arc<BrokerClient> {
        let (worker, broker) = UnixStream::pair().unwrap();
        std::thread::spawn(move || serve(broker, Some(Arc::new(Echo)), None));
        BrokerClient::new(worker).unwrap()
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
    fn plans_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::plans::HostPlans::new(
            tmp.path().join("plans"),
            tmp.path().join("ws.list"),
        ));
        let (worker, broker) = UnixStream::pair().unwrap();
        std::thread::spawn(move || serve(broker, None, Some(store)));
        let remote = BrokerClient::new(worker).unwrap();
        assert!(remote.write("p.md", "one\n").unwrap().created);
        assert_eq!(
            remote
                .edit("p.md", "one", "two", false)
                .unwrap()
                .replacements,
            1
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("plans/p.md")).unwrap(),
            "two\n"
        );
        let err = PlanStore::write(&*remote, "../x.md", "x").unwrap_err();
        assert!(err.contains("not a plan file name"), "{err}");
        let err = remote
            .run("echo", &[], &BTreeMap::new(), "/", &Cancel::new())
            .unwrap_err();
        assert!(err.0.contains("no host actions"), "{err}");
    }

    #[test]
    fn broker_rejects_garbage_and_worker_notices() {
        let (mut worker, broker) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || serve(broker, Some(Arc::new(Echo)), None));
        worker
            .write_all(b"{\"op\":\"exec\",\"argv\":[\"/bin/sh\"]}\n")
            .unwrap();
        assert!(handle.join().unwrap().is_err());

        let (worker, broker) = UnixStream::pair().unwrap();
        drop(broker);
        let remote = BrokerClient::new(worker).unwrap();
        let err = remote
            .run("echo", &[], &BTreeMap::new(), "/", &Cancel::new())
            .unwrap_err();
        assert!(err.0.contains("broker is gone"), "{err}");
    }
}
