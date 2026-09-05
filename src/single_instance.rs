//! Per-user desktop single-instance coordination over loopback TCP.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const IPC_SCHEMA_VERSION: u32 = 2;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const ACK: u8 = 0x06;
const IPC_TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub schema_version: u32,
    pub paths: Vec<PathBuf>,
    pub focus_window: bool,
    #[serde(default)]
    pub auth_token: String,
}

impl OpenRequest {
    #[cfg(test)]
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            schema_version: IPC_SCHEMA_VERSION,
            paths,
            focus_window: true,
            auth_token: String::new(),
        }
    }

    fn with_auth_token(paths: Vec<PathBuf>, auth_token: String) -> Self {
        Self {
            schema_version: IPC_SCHEMA_VERSION,
            paths,
            focus_window: true,
            auth_token,
        }
    }
}

pub enum Acquisition {
    Primary(Receiver<OpenRequest>),
    Forwarded,
    Unavailable(String),
}

pub fn acquire(paths: Vec<PathBuf>) -> Acquisition {
    let address = ipc_address();
    let token = match load_or_create_ipc_token() {
        Ok(token) => token,
        Err(error) => {
            return Acquisition::Unavailable(format!("无法初始化单实例认证：{error}"));
        }
    };
    let request = OpenRequest::with_auth_token(paths, token.clone());
    match TcpListener::bind(address) {
        Ok(listener) => Acquisition::Primary(start_listener(listener, token)),
        Err(bind_error) => match forward_request(address, &request) {
            Ok(()) => Acquisition::Forwarded,
            Err(forward_error) => Acquisition::Unavailable(format!(
                "无法连接现有窗口（端口 {}）：{forward_error}；监听失败：{bind_error}",
                address.port()
            )),
        },
    }
}

fn ipc_token_path() -> PathBuf {
    crate::storage::config_dir().join("state").join("ipc.token")
}

fn read_token_file(path: &Path) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((IPC_TOKEN_BYTES * 2 + 1) as u64)
        .read_to_end(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "IPC token is not UTF-8"))
}

fn valid_token(token: &str) -> bool {
    token.len() == IPC_TOKEN_BYTES * 2 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn load_or_create_ipc_token() -> io::Result<String> {
    let path = ipc_token_path();
    if path.exists() {
        match read_token_file(&path) {
            Ok(token) if valid_token(&token) => return Ok(token),
            Ok(_) => {
                crate::storage::quarantine_corrupt(&path);
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                crate::storage::quarantine_corrupt(&path);
            }
            Err(error) => return Err(error),
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let token = rand::random::<[u8; IPC_TOKEN_BYTES]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let token = read_token_file(&path)?;
            if valid_token(&token) {
                Ok(token)
            } else {
                crate::storage::quarantine_corrupt(&path);
                load_or_create_ipc_token()
            }
        }
        Err(error) => Err(error),
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
                if let Ok(request) = read_request(&mut stream)
                    && request.auth_token == expected_token
                    && sender.send(request).is_ok()
                {
                    let _ = stream.write_all(&[ACK]);
                    let _ = stream.flush();
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
                return if acknowledgement[0] == ACK {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "existing process returned an invalid acknowledgement",
                    ))
                };
            }
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("existing process did not respond")))
}

fn write_request(stream: &mut impl Write, request: &OpenRequest) -> io::Result<()> {
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
            paths: Vec::new(),
            focus_window: true,
            auth_token: String::new(),
        };
        let mut bytes = Vec::new();
        write_request(&mut bytes, &request).unwrap();
        assert!(read_request(&mut bytes.as_slice()).is_err());

        let mut oversized = ((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes().to_vec();
        oversized.extend_from_slice(b"{}");
        assert!(read_request(&mut oversized.as_slice()).is_err());
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
}
