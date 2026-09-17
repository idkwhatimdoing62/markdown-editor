//! Per-user desktop single-instance coordination over loopback TCP.
//!
//! The listener accepts only requests carrying the shared token stored in this
//! user's configuration directory, so another local user or an unrelated
//! program that happens to own the port cannot make this instance open files.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const IPC_SCHEMA_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_PATHS: usize = 256;
const MIN_TOKEN_LEN: usize = 32;
const ACK: u8 = 0x06;
const NACK: u8 = 0x15;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub schema_version: u32,
    pub paths: Vec<PathBuf>,
    pub focus_window: bool,
    /// Shared secret of this user's installation; see [`token`]. Required:
    /// a message without it cannot be authorised anyway.
    pub token: String,
}

impl OpenRequest {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            schema_version: IPC_SCHEMA_VERSION,
            paths,
            focus_window: true,
            token: token().to_string(),
        }
    }
}

/// Path of the token shared by every instance of this user.
fn token_path() -> PathBuf {
    crate::storage::config_dir().join("state").join("ipc-token")
}

/// The per-user IPC token, created on first use.
fn token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| load_or_create_token(&token_path()))
}

fn load_or_create_token(path: &Path) -> String {
    if let Some(existing) = read_token(path) {
        return existing;
    }
    let candidate = random_token();
    match write_token_once(path, &candidate) {
        Ok(()) => candidate,
        // Another instance created the file first: adopt its token.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match read_token(path) {
                Some(existing) => existing,
                // The file exists but is unusable; replace it so both sides
                // agree again instead of failing every later launch.
                None => {
                    let _ = crate::storage::write_atomic(path, candidate.as_bytes());
                    candidate
                }
            }
        }
        Err(_) => candidate,
    }
}

fn read_token(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let token = text.trim();
    (token.len() >= MIN_TOKEN_LEN).then(|| token.to_string())
}

fn write_token_once(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(token.as_bytes())
}

/// Random token without an extra dependency: `RandomState` is seeded from the
/// operating system, and two hashes of it yield 128 bits of output.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let state = RandomState::new();
    let mut token = String::with_capacity(MIN_TOKEN_LEN);
    for index in 0u64..2 {
        let mut hasher = state.build_hasher();
        hasher.write_u64(index);
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

fn tokens_match(expected: &str, provided: &str) -> bool {
    // A missing or truncated expected token means the listener's own token is
    // unusable; treating it as a match would accept anything.
    expected.len() >= MIN_TOKEN_LEN
        && expected.len() == provided.len()
        && expected
            .bytes()
            .zip(provided.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

pub enum Acquisition {
    Primary(Receiver<OpenRequest>),
    Forwarded,
    Unavailable(String),
}

pub fn acquire(paths: Vec<PathBuf>) -> Acquisition {
    let address = ipc_address();
    // Read (or create) the token before binding: the listener needs it and a
    // forwarding client must use the same value.
    let request = OpenRequest::new(paths);
    match TcpListener::bind(address) {
        Ok(listener) => Acquisition::Primary(start_listener(listener, request.token.clone())),
        Err(bind_error) => match forward_request(address, &request) {
            Ok(()) => Acquisition::Forwarded,
            Err(forward_error) => Acquisition::Unavailable(format!(
                "无法连接现有窗口（端口 {}）：{forward_error}；监听失败：{bind_error}",
                address.port()
            )),
        },
    }
}

fn ipc_address() -> SocketAddrV4 {
    // APPDATA/HOME is user-specific. A stable hash avoids forwarding one
    // desktop user's file paths to another user logged into the same machine.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in crate::storage::config_dir().to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let port = 40_000 + (hash % 10_000) as u16;
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
}

fn start_listener(listener: TcpListener, expected_token: String) -> Receiver<OpenRequest> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("markdown-editor-single-instance".to_string())
        .spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut stream) = connection else {
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                match read_request(&mut stream) {
                    Ok(request) if tokens_match(&expected_token, &request.token) => {
                        if sender.send(request).is_ok() {
                            let _ = stream.write_all(&[ACK]);
                            let _ = stream.flush();
                        }
                    }
                    // Answer with a negative acknowledgement so an unauthorised
                    // client stops retrying instead of waiting out its budget.
                    Ok(_) => {
                        let _ = stream.write_all(&[NACK]);
                        let _ = stream.flush();
                    }
                    Err(_) => {}
                }
            }
        })
        .expect("single-instance listener thread should start");
    receiver
}

fn forward_request(address: SocketAddrV4, request: &OpenRequest) -> io::Result<()> {
    let mut last_error = None;
    for _ in 0..40 {
        match TcpStream::connect_timeout(&address.into(), Duration::from_millis(150)) {
            Ok(mut stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                stream.set_write_timeout(Some(Duration::from_secs(3)))?;
                write_request(&mut stream, request)?;
                let mut acknowledgement = [0u8; 1];
                stream.read_exact(&mut acknowledgement)?;
                return match acknowledgement[0] {
                    ACK => Ok(()),
                    NACK => Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "现有窗口拒绝了连接（实例令牌不匹配），请关闭其他窗口后重试",
                    )),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "existing process returned an invalid acknowledgement",
                    )),
                };
            }
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("existing process did not respond")))
}

fn write_request(stream: &mut impl Write, request: &OpenRequest) -> io::Result<()> {
    if request.paths.len() > MAX_PATHS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "open request contains too many paths",
        ));
    }
    let payload = serde_json::to_vec(request).map_err(io::Error::other)?;
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "open request exceeds the IPC limit",
        ));
    }
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()
}

fn read_request(stream: &mut impl Read) -> io::Result<OpenRequest> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC message length",
        ));
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    let request: OpenRequest = serde_json::from_slice(&payload).map_err(io::Error::other)?;
    if request.schema_version != IPC_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported IPC schema version",
        ));
    }
    if request.paths.len() > MAX_PATHS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "open request contains too many paths",
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_round_trips_multiple_unicode_paths() {
        let request = OpenRequest::new(vec![
            PathBuf::from("C:/笔记/一.md"),
            PathBuf::from("C:/notes/two.markdown"),
        ]);
        let mut bytes = Vec::new();
        write_request(&mut bytes, &request).unwrap();
        assert_eq!(read_request(&mut bytes.as_slice()).unwrap(), request);
    }

    #[test]
    fn rejects_unknown_schema_and_oversized_frames() {
        let request = OpenRequest {
            schema_version: 99,
            ..OpenRequest::new(Vec::new())
        };
        let mut bytes = Vec::new();
        write_request(&mut bytes, &request).unwrap();
        assert!(read_request(&mut bytes.as_slice()).is_err());

        let mut oversized = ((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes().to_vec();
        oversized.extend_from_slice(b"{}");
        assert!(read_request(&mut oversized.as_slice()).is_err());
    }

    #[test]
    fn rejects_requests_with_too_many_paths() {
        let request = OpenRequest::new(vec![PathBuf::from("note.md"); MAX_PATHS + 1]);
        let mut bytes = Vec::new();
        assert!(write_request(&mut bytes, &request).is_err());

        let payload = serde_json::to_vec(&request).unwrap();
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&payload);
        assert!(read_request(&mut frame.as_slice()).is_err());
    }

    #[test]
    fn per_user_address_is_loopback_and_stable() {
        assert_eq!(ipc_address().ip(), &Ipv4Addr::LOCALHOST);
        assert_eq!(ipc_address(), ipc_address());
        assert!((40_000..50_000).contains(&ipc_address().port()));
    }

    #[test]
    fn secondary_process_forwards_paths_and_waits_for_acknowledgement() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = match listener.local_addr().unwrap() {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => unreachable!("test listener is IPv4"),
        };
        let request = OpenRequest::new(vec![PathBuf::from("C:/笔记/转发.md")]);
        let expected = request.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(read_request(&mut stream).unwrap(), expected);
            stream.write_all(&[ACK]).unwrap();
        });

        forward_request(address, &request).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn 令牌比较拒绝伪造或不完整的值() {
        let expected = "a".repeat(MIN_TOKEN_LEN);
        assert!(tokens_match(&expected, &expected));
        assert!(!tokens_match(&expected, &"b".repeat(MIN_TOKEN_LEN)));
        assert!(!tokens_match(&expected, "a"));
        assert!(!tokens_match(&expected, ""));
        assert!(!tokens_match("", ""));
    }

    #[test]
    fn 用户令牌稳定且足够长() {
        let first = token().to_string();
        assert!(first.len() >= MIN_TOKEN_LEN);
        assert_eq!(first, token());
        assert_eq!(read_token(&token_path()).as_deref(), Some(first.as_str()));
    }

    #[test]
    fn 未授权的请求得到否定应答而不是被转发() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = match listener.local_addr().unwrap() {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => unreachable!("test listener is IPv4"),
        };
        let received = start_listener(listener, "expected-token".to_string());
        let request = OpenRequest {
            token: "forged-token".to_string(),
            ..OpenRequest::new(vec![PathBuf::from("C:/笔记/伪造.md")])
        };

        let error = forward_request(address, &request).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(received.try_recv().is_err(), "未授权的请求不得进入窗口");
    }
}
