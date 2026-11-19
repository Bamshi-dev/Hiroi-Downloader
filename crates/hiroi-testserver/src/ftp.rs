//! A small FTP server for tests: just the commands a download needs
//! (RFC 959, with SIZE, MDTM, and REST STREAM from RFC 3659, and EPSV
//! from RFC 2428). Passive mode only; files come from memory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, Default)]
pub struct FtpBehavior {
    /// Speed limit per data connection.
    pub bytes_per_sec: Option<u64>,
    /// Answer 421 to logins beyond this many open sessions.
    pub max_sessions: Option<usize>,
    /// Only this user and password may log in; anyone when `None`.
    pub login: Option<(String, String)>,
    /// Leave REST out of FEAT and refuse it.
    pub no_rest: bool,
}

#[derive(Debug, Default)]
struct Counters {
    sessions: AtomicUsize,
    peak_sessions: AtomicUsize,
    bytes_served: AtomicU64,
    refused: AtomicUsize,
}

#[derive(Debug)]
pub struct FtpServer {
    port: u16,
    files: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
    counters: Arc<Counters>,
    task: tokio::task::JoinHandle<()>,
}

impl FtpServer {
    pub async fn start(behavior: FtpBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let files = Arc::new(Mutex::new(HashMap::new()));
        let counters = Arc::new(Counters::default());
        let behavior = Arc::new(behavior);
        let task = {
            let files = Arc::clone(&files);
            let counters = Arc::clone(&counters);
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let session = Session {
                        files: Arc::clone(&files),
                        counters: Arc::clone(&counters),
                        behavior: Arc::clone(&behavior),
                    };
                    tokio::spawn(session.run(socket));
                }
            })
        };
        Self {
            port,
            files,
            counters,
            task,
        }
    }

    pub fn put(&self, name: &str, data: Vec<u8>) {
        self.files
            .lock()
            .unwrap()
            .insert(format!("/{name}"), Arc::new(data));
    }

    /// `ftp://127.0.0.1:port/name`.
    pub fn url(&self, name: &str) -> String {
        format!("ftp://127.0.0.1:{}/{name}", self.port)
    }

    pub fn peak_sessions(&self) -> usize {
        self.counters.peak_sessions.load(Ordering::Relaxed)
    }

    pub fn bytes_served(&self) -> u64 {
        self.counters.bytes_served.load(Ordering::Relaxed)
    }

    /// Logins answered with 421 because of `max_sessions`.
    pub fn refused(&self) -> usize {
        self.counters.refused.load(Ordering::Relaxed)
    }
}

impl Drop for FtpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Session {
    files: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
    counters: Arc<Counters>,
    behavior: Arc<FtpBehavior>,
}

/// Counts a session for as long as it lives.
struct Open<'a>(&'a Counters);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Session {
    async fn run(self, socket: TcpStream) {
        let (read, mut write) = socket.into_split();
        let mut lines = BufReader::new(read).lines();
        let open = self.counters.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        let _open = Open(&self.counters);
        if self.behavior.max_sessions.is_some_and(|max| open > max) {
            self.counters.refused.fetch_add(1, Ordering::Relaxed);
            let _ = reply(&mut write, "421 Too many connections").await;
            return;
        }
        self.counters
            .peak_sessions
            .fetch_max(open, Ordering::Relaxed);
        if reply(&mut write, "220 hiroi test server").await.is_err() {
            return;
        }

        let mut user = String::new();
        let mut logged_in = false;
        let mut offset = 0u64;
        let mut passive: Option<TcpListener> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let (command, argument) = line.split_once(' ').unwrap_or((&line, ""));
            let command = command.to_ascii_uppercase();
            let answer = match command.as_str() {
                "USER" => {
                    user = argument.to_string();
                    "331 Password please".to_string()
                }
                "PASS" => match &self.behavior.login {
                    Some((name, password)) if *name != user || password != argument => {
                        "530 Login incorrect".to_string()
                    }
                    _ => {
                        logged_in = true;
                        "230 Logged in".to_string()
                    }
                },
                "QUIT" => {
                    let _ = reply(&mut write, "221 Bye").await;
                    return;
                }
                _ if !logged_in => "530 Please log in".to_string(),
                "FEAT" => {
                    let rest = if self.behavior.no_rest {
                        ""
                    } else {
                        " REST STREAM\r\n"
                    };
                    format!("211-Features\r\n SIZE\r\n MDTM\r\n EPSV\r\n{rest}211 End")
                }
                "SYST" => "215 UNIX Type: L8".to_string(),
                "TYPE" | "NOOP" | "OPTS" => "200 OK".to_string(),
                "PWD" => "257 \"/\"".to_string(),
                "SIZE" => match self.file(argument) {
                    Some(data) => format!("213 {}", data.len()),
                    None => "550 No such file".to_string(),
                },
                "MDTM" => match self.file(argument) {
                    Some(_) => "213 20260102030405".to_string(),
                    None => "550 No such file".to_string(),
                },
                "REST" if self.behavior.no_rest => "502 REST not implemented".to_string(),
                "REST" => match argument.parse() {
                    Ok(value) => {
                        offset = value;
                        format!("350 Restarting at {value}")
                    }
                    Err(_) => "501 Bad offset".to_string(),
                },
                "EPSV" | "PASV" => match TcpListener::bind("127.0.0.1:0").await {
                    Ok(listener) => {
                        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
                        passive = Some(listener);
                        if command == "EPSV" {
                            format!("229 Entering Extended Passive Mode (|||{port}|)")
                        } else {
                            format!(
                                "227 Entering Passive Mode (127,0,0,1,{},{})",
                                port >> 8,
                                port & 0xff
                            )
                        }
                    }
                    Err(_) => "425 Cannot open data connection".to_string(),
                },
                "RETR" => {
                    let start = std::mem::take(&mut offset);
                    let (Some(listener), Some(data)) = (passive.take(), self.file(argument)) else {
                        let _ = reply(&mut write, "550 No such file or no data connection").await;
                        continue;
                    };
                    if reply(&mut write, "150 Sending").await.is_err() {
                        return;
                    }
                    let sent = match listener.accept().await {
                        Ok((data_socket, _)) => self.send(data_socket, &data, start).await,
                        Err(_) => false,
                    };
                    if sent {
                        "226 Done"
                    } else {
                        "426 Transfer aborted"
                    }
                    .to_string()
                }
                _ => "502 Not implemented".to_string(),
            };
            if reply(&mut write, &answer).await.is_err() {
                return;
            }
        }
    }

    fn file(&self, path: &str) -> Option<Arc<Vec<u8>>> {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        self.files.lock().unwrap().get(&path).cloned()
    }

    /// Sends the file from `start`; false when the client went away.
    async fn send(&self, mut socket: TcpStream, data: &[u8], start: u64) -> bool {
        let start = usize::try_from(start).unwrap_or(usize::MAX).min(data.len());
        for chunk in data[start..].chunks(CHUNK) {
            if let Some(rate) = self.behavior.bytes_per_sec {
                let pause = Duration::from_secs_f64(chunk.len() as f64 / rate as f64);
                tokio::time::sleep(pause).await;
            }
            if socket.write_all(chunk).await.is_err() {
                return false;
            }
            self.counters
                .bytes_served
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        socket.shutdown().await.is_ok()
    }
}

async fn reply(write: &mut (impl AsyncWrite + Unpin), text: &str) -> std::io::Result<()> {
    write.write_all(format!("{text}\r\n").as_bytes()).await
}
