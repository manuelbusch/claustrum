//! Host commands: functions implemented in the Claustrum process that appear
//! as executables (`/bin/<name>`) inside the guest.
//!
//! Mechanism: a small WASI shim (`assets/hostcmd.wasm`, source in `shim/`) is
//! registered under every host command name. When bash runs `host test`,
//! the shim opens `/.claustrum/cmd/host`, a virtual file served by
//! [`HostCommandFs`], writes its arguments and working directory, and reads
//! the result back. The host runs the [`HostCommand`] synchronously while
//! serving that read. Output goes through the shim's own stdout/stderr, so
//! pipes and redirections in bash behave as usual.

use std::{
    io::{self, SeekFrom},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use futures::future::BoxFuture;
use virtual_fs::{
    AsyncRead, AsyncSeek, AsyncWrite, DirEntry, FileOpener, FileSystem, FileType, FsError,
    Metadata, OpenOptions, OpenOptionsConfig, ReadDir, VirtualFile,
};

/// Guest mount point of the request channel file system.
pub const MOUNT: &str = "/.claustrum";
const CMD_DIR: &str = "/cmd";

/// The shim module, built by `scripts/build-shim.sh`.
pub const SHIM_WASM: &[u8] = include_bytes!("../assets/hostcmd.wasm");

/// Result of a host command invocation.
#[derive(Clone, Debug, Default)]
pub struct HostOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

impl HostOutput {
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: Vec::new(),
            code: 0,
        }
    }

    pub fn fail(code: i32, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: stderr.into(),
            code,
        }
    }
}

/// Tells a running host command that the guest process it serves was killed
/// (for instance by the Bash timeout), so it should stop whatever it started
/// on the host and return. Cheap to clone; all clones observe the same flag.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// A guard that sets the flag when it is dropped: held by a future
    /// whose caller may drop it early (a cancelled MCP request), so that the
    /// host work it started stops as well.
    pub fn on_drop(&self) -> CancelOnDrop {
        CancelOnDrop(self.clone())
    }
}

/// See [`Cancel::on_drop`]. Cancelling after the work finished is harmless.
#[derive(Debug)]
pub struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// What the guest handed to a host command invocation.
#[derive(Clone, Debug)]
pub struct Invocation {
    /// Guest working directory (absolute guest path).
    pub cwd: String,
    /// Empty unless [`HostCommand::wants_stdin`] returned true.
    pub stdin: Vec<u8>,
    /// Set when the guest process is killed while the command runs.
    pub cancel: Cancel,
}

/// A command implemented on the host and exposed inside the guest.
pub trait HostCommand: Send + Sync + std::fmt::Debug + 'static {
    /// Command name, registered at `/bin/<name>`. Letters, digits, `-`, `_`.
    fn name(&self) -> &str;

    /// Whether this invocation reads stdin. Only then does the shim read its
    /// stdin to EOF and forward it; other invocations never block on an open
    /// pipe.
    fn wants_stdin(&self, _args: &[String]) -> bool {
        false
    }

    /// Run the command. `args` excludes argv[0]. Runs on the guest's thread
    /// while it waits for the result, so long-running commands must watch
    /// `invocation.cancel`. Must not panic.
    fn run(&self, args: &[String], invocation: &Invocation) -> HostOutput;
}

/// Exit code in a response header that asks the shim for stdin.
const NEED_STDIN: i32 = -2;
/// Bytes a guest may write to one host command channel (request plus
/// stdin). Inputs are at most 64 KiB each; this only stops a guest from
/// filling the host's memory through the channel.
const MAX_CHANNEL_BYTES: usize = 16 * 1024 * 1024;

/// Write the directory package (`wasmer.toml` + shim) that registers every
/// host command as a guest executable. Returns the package directory.
pub(crate) fn write_shim_package(
    cache_dir: &Path,
    commands: &[Arc<dyn HostCommand>],
) -> std::io::Result<PathBuf> {
    use std::fmt::Write as _;
    let mut manifest = String::from(
        "[package]\nname = \"claustrum/hostcmd\"\nversion = \"0.1.0\"\n\n\
         [[module]]\nname = \"hostcmd\"\nsource = \"./hostcmd.wasm\"\nabi = \"wasi\"\n",
    );
    for c in commands {
        // Interpolated into TOML; host command names are validated where
        // they are declared (`action::is_action_name`).
        debug_assert!(
            c.name()
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch)),
            "host command name {:?}",
            c.name()
        );
        write!(
            manifest,
            "\n[[command]]\nname = \"{0}\"\nmodule = \"hostcmd\"\nrunner = \"https://webc.org/runner/wasi\"\n\n\
             [command.annotations.wasi]\natom = \"hostcmd\"\n",
            c.name()
        )
        .expect("writing to a String cannot fail");
    }
    // Key the directory by content so a changed shim or command set gets a
    // fresh package identity (BinaryPackage::from_dir hashes the path).
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&manifest, &mut hasher);
    std::hash::Hash::hash(SHIM_WASM, &mut hasher);
    let dir = cache_dir.join(format!(
        "hostcmd-{:016x}",
        std::hash::Hasher::finish(&hasher)
    ));
    std::fs::create_dir_all(&dir)?;
    let wasm = dir.join("hostcmd.wasm");
    if std::fs::read(&wasm).ok().as_deref() != Some(SHIM_WASM) {
        std::fs::write(&wasm, SHIM_WASM)?;
    }
    std::fs::write(dir.join("wasmer.toml"), manifest)?;
    Ok(dir)
}

/// File system mounted at [`MOUNT`]: `/cmd/<name>` are the request channels.
#[derive(Debug)]
pub(crate) struct HostCommandFs {
    commands: Vec<Arc<dyn HostCommand>>,
    cancel: Cancel,
}

impl HostCommandFs {
    /// One instance per guest process, so that `cancel` identifies exactly
    /// the invocations started by that process.
    pub(crate) fn new(commands: Vec<Arc<dyn HostCommand>>, cancel: Cancel) -> Self {
        Self { commands, cancel }
    }

    fn command(&self, path: &Path) -> Option<Arc<dyn HostCommand>> {
        let rest = path.strip_prefix(CMD_DIR).ok()?;
        let name = rest.to_str()?;
        self.commands.iter().find(|c| c.name() == name).cloned()
    }

    fn dir_meta() -> Metadata {
        Metadata {
            ft: FileType {
                dir: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn file_meta() -> Metadata {
        Metadata {
            ft: FileType {
                file: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

impl FileSystem for HostCommandFs {
    fn readlink(&self, _path: &Path) -> virtual_fs::Result<PathBuf> {
        Err(FsError::InvalidInput)
    }

    fn read_dir(&self, path: &Path) -> virtual_fs::Result<ReadDir> {
        if path == Path::new("/") {
            return Ok(ReadDir::new(vec![DirEntry {
                path: PathBuf::from(CMD_DIR),
                metadata: Ok(Self::dir_meta()),
            }]));
        }
        if path == Path::new(CMD_DIR) {
            return Ok(ReadDir::new(
                self.commands
                    .iter()
                    .map(|c| DirEntry {
                        path: Path::new(CMD_DIR).join(c.name()),
                        metadata: Ok(Self::file_meta()),
                    })
                    .collect(),
            ));
        }
        Err(FsError::EntryNotFound)
    }

    fn create_dir(&self, _path: &Path) -> virtual_fs::Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn remove_dir(&self, _path: &Path) -> virtual_fs::Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn rename<'a>(
        &'a self,
        _from: &'a Path,
        _to: &'a Path,
    ) -> BoxFuture<'a, virtual_fs::Result<()>> {
        Box::pin(async { Err(FsError::PermissionDenied) })
    }

    fn metadata(&self, path: &Path) -> virtual_fs::Result<Metadata> {
        if path == Path::new("/") || path == Path::new(CMD_DIR) {
            Ok(Self::dir_meta())
        } else if self.command(path).is_some() {
            Ok(Self::file_meta())
        } else {
            Err(FsError::EntryNotFound)
        }
    }

    fn symlink_metadata(&self, path: &Path) -> virtual_fs::Result<Metadata> {
        self.metadata(path)
    }

    fn remove_file(&self, _path: &Path) -> virtual_fs::Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn new_open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }
}

impl FileOpener for HostCommandFs {
    fn open(
        &self,
        path: &Path,
        _conf: &OpenOptionsConfig,
    ) -> virtual_fs::Result<Box<dyn VirtualFile + Send + Sync + 'static>> {
        let command = self.command(path).ok_or(FsError::EntryNotFound)?;
        Ok(Box::new(ChannelFile {
            command,
            cancel: self.cancel.clone(),
            phase: Phase::Request,
            buf: Vec::new(),
            request_len: 0,
            response: None,
            read_pos: 0,
        }))
    }
}

/// One invocation. Bytes written are the request (then, if asked for, the
/// stdin chunk); the first read after each write phase produces a response.
#[derive(Debug)]
struct ChannelFile {
    command: Arc<dyn HostCommand>,
    cancel: Cancel,
    phase: Phase,
    /// Request bytes (args + cwd), then the stdin chunk appended.
    buf: Vec<u8>,
    /// Length of the args+cwd part once parsed.
    request_len: usize,
    response: Option<Arc<Vec<u8>>>,
    /// Position within `response`; reset whenever a new response is produced.
    read_pos: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Request,
    Stdin,
    Done,
}

impl ChannelFile {
    fn respond(&mut self, output: &HostOutput) -> Arc<Vec<u8>> {
        let r = Arc::new(encode_response(output));
        self.response = Some(Arc::clone(&r));
        self.read_pos = 0;
        r
    }

    fn run(&mut self, args: Vec<String>, cwd: String, stdin: Vec<u8>) -> Arc<Vec<u8>> {
        let name = self.command.name().to_owned();
        let invocation = Invocation {
            cwd,
            stdin,
            cancel: self.cancel.clone(),
        };
        let output = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.command.run(&args, &invocation)
        })) {
            Ok(out) => out,
            Err(_) => HostOutput::fail(70, format!("{name}: internal error\n")),
        };
        self.phase = Phase::Done;
        self.respond(&output)
    }

    fn response(&mut self) -> Arc<Vec<u8>> {
        if let Some(r) = &self.response {
            return Arc::clone(r);
        }
        match self.phase {
            Phase::Request => match decode_request(&self.buf) {
                Ok((args, cwd, consumed)) => {
                    self.request_len = consumed;
                    if self.command.wants_stdin(&args) {
                        self.phase = Phase::Stdin;
                        self.respond(&HostOutput::fail(NEED_STDIN, ""))
                    } else {
                        self.run(args, cwd, Vec::new())
                    }
                }
                Err(e) => {
                    self.phase = Phase::Done;
                    self.respond(&HostOutput::fail(
                        127,
                        format!("host command: bad request: {e}\n"),
                    ))
                }
            },
            Phase::Stdin => {
                let (args, cwd, _) = decode_request(&self.buf).expect("parsed before");
                let stdin = decode_stdin(&self.buf[self.request_len..]);
                self.run(args, cwd, stdin)
            }
            // The response is set once done; answer rather than panic inside
            // a WASI syscall if that ever changes.
            Phase::Done => {
                let out = HostOutput::fail(70, "host command: no response\n");
                Arc::new(encode_response(&out))
            }
        }
    }
}

/// Parse args and cwd; returns the number of bytes consumed.
fn decode_request(buf: &[u8]) -> Result<(Vec<String>, String, usize), String> {
    let mut pos = 0;
    let u32_at = |pos: &mut usize| -> Result<u32, String> {
        let end = pos.checked_add(4).ok_or("truncated")?;
        let b = buf.get(*pos..end).ok_or("truncated")?;
        *pos = end;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    };
    let argc = u32_at(&mut pos)? as usize;
    if argc > 100_000 {
        return Err("too many arguments".into());
    }
    let mut args = Vec::with_capacity(argc);
    let string_at = |pos: &mut usize| -> Result<String, String> {
        let len = u32_at(pos)? as usize;
        let end = pos.checked_add(len).ok_or("truncated")?;
        let b = buf.get(*pos..end).ok_or("truncated")?;
        *pos = end;
        Ok(String::from_utf8_lossy(b).into_owned())
    };
    for _ in 0..argc {
        args.push(string_at(&mut pos)?);
    }
    let cwd = string_at(&mut pos)?;
    Ok((args, cwd, pos))
}

/// `u32 len + bytes`; a short chunk yields what arrived.
fn decode_stdin(buf: &[u8]) -> Vec<u8> {
    if buf.len() < 4 {
        return Vec::new();
    }
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    buf[4..].iter().take(len).copied().collect()
}

fn encode_response(out: &HostOutput) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + out.stdout.len() + out.stderr.len());
    buf.extend_from_slice(&out.code.to_le_bytes());
    buf.extend_from_slice(&(out.stdout.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(out.stderr.len() as u32).to_le_bytes());
    buf.extend_from_slice(&out.stdout);
    buf.extend_from_slice(&out.stderr);
    buf
}

impl AsyncWrite for ChannelFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.phase {
            Phase::Done => return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            // New bytes after the "need stdin" answer start the stdin phase.
            Phase::Stdin => self.response = None,
            Phase::Request => {}
        }
        if self.buf.len().saturating_add(buf.len()) > MAX_CHANNEL_BYTES {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("host command request larger than {MAX_CHANNEL_BYTES} bytes"),
            )));
        }
        self.buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for ChannelFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        dst: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // The descriptor offset WASI maintains is meaningless for a channel;
        // reads are sequential within the current response.
        let response = self.response();
        let start = self.read_pos.min(response.len());
        let n = dst.remaining().min(response.len() - start);
        dst.put_slice(&response[start..start + n]);
        self.read_pos = start + n;
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for ChannelFile {
    fn start_seek(self: Pin<&mut Self>, _pos: SeekFrom) -> io::Result<()> {
        // Seeks are accepted and ignored; see `poll_read`.
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.read_pos as u64))
    }
}

impl VirtualFile for ChannelFile {
    fn last_accessed(&self) -> u64 {
        0
    }

    fn last_modified(&self) -> u64 {
        0
    }

    fn created_time(&self) -> u64 {
        0
    }

    fn size(&self) -> u64 {
        (self.buf.len() + self.response.as_ref().map(|r| r.len()).unwrap_or(0)) as u64
    }

    fn set_len(&mut self, _new_size: u64) -> virtual_fs::Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn unlink(&mut self) -> virtual_fs::Result<()> {
        Ok(())
    }

    fn poll_read_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        // Always readable: the first read produces the response.
        Poll::Ready(Ok(8192))
    }

    fn poll_write_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(8192))
    }
}
