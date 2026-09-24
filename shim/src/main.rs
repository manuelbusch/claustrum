//! Forwards an invocation to the Claustrum host and relays the result.
//!
//! The host mounts a virtual file system at `/.claustrum`; opening
//! `/.claustrum/cmd/<name>` yields a fresh request channel. The protocol is
//! length-prefixed and little-endian:
//!
//! request:  u32 argc, then per argument (u32 len, bytes), then u32 len + cwd
//! response: i32 exit code, u32 stdout len, u32 stderr len, stdout, stderr
//!
//! If the host answers with exit code -2 and no output, the command wants
//! stdin: the shim reads its stdin to EOF, sends `u32 len + bytes`, and
//! reads the real response. Commands that do not need stdin never block on
//! an open pipe this way.
//!
//! The command name is argv[0]'s basename, so one module serves every host
//! command registered as `/bin/<name>`.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    process::exit,
};

const CHANNEL_DIR: &str = "/.claustrum/cmd";
/// Exit code the host uses to ask for stdin.
const NEED_STDIN: i32 = -2;

fn read_response(channel: &mut std::fs::File, name: &str) -> Vec<u8> {
    let mut response = Vec::new();
    if let Err(e) = channel.read_to_end(&mut response) {
        eprintln!("{name}: cannot read host response ({e})");
        exit(127);
    }
    if response.len() < 12 {
        eprintln!("{name}: malformed host response");
        exit(127);
    }
    response
}

fn main() {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let name = argv0.rsplit('/').next().unwrap_or(&argv0).to_owned();
    let args: Vec<String> = args.collect();
    let cwd = std::env::var("PWD").unwrap_or_else(|_| "/".to_owned());

    let mut request = Vec::new();
    push_u32(&mut request, args.len() as u32);
    for a in &args {
        push_bytes(&mut request, a.as_bytes());
    }
    push_bytes(&mut request, cwd.as_bytes());

    let path = format!("{CHANNEL_DIR}/{name}");
    let mut channel = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{name}: host command channel unavailable ({e})");
            exit(127);
        }
    };
    if let Err(e) = channel.write_all(&request).and_then(|_| channel.flush()) {
        eprintln!("{name}: cannot send request to host ({e})");
        exit(127);
    }

    let mut response = read_response(&mut channel, &name);
    let code = i32::from_le_bytes(response[0..4].try_into().unwrap());
    if code == NEED_STDIN {
        let mut stdin = Vec::new();
        if let Err(e) = std::io::stdin().lock().read_to_end(&mut stdin) {
            eprintln!("{name}: cannot read stdin ({e})");
            exit(127);
        }
        let mut chunk = Vec::with_capacity(4 + stdin.len());
        push_bytes(&mut chunk, &stdin);
        if let Err(e) = channel.write_all(&chunk).and_then(|_| channel.flush()) {
            eprintln!("{name}: cannot send stdin to host ({e})");
            exit(127);
        }
        response = read_response(&mut channel, &name);
    }
    drop(channel);

    let code = i32::from_le_bytes(response[0..4].try_into().unwrap());
    let out_len = u32::from_le_bytes(response[4..8].try_into().unwrap()) as usize;
    let err_len = u32::from_le_bytes(response[8..12].try_into().unwrap()) as usize;
    let body = &response[12..];
    if body.len() < out_len + err_len {
        eprintln!("{name}: truncated host response");
        exit(127);
    }
    let (out, err) = (&body[..out_len], &body[out_len..out_len + err_len]);

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let _ = stdout.write_all(out);
    let _ = stdout.flush();
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = stderr.write_all(err);
    let _ = stderr.flush();
    exit(code);
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    push_u32(buf, b.len() as u32);
    buf.extend_from_slice(b);
}
