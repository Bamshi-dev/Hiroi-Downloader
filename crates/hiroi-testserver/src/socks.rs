//! A SOCKS5 proxy for tests (RFC 1928): no login, CONNECT only. It
//! records every target, so a test can check that traffic went through
//! it and whether host names or addresses were sent.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug)]
pub struct SocksProxy {
    port: u16,
    targets: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl SocksProxy {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let targets = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&targets);
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                tokio::spawn(relay(client, Arc::clone(&seen)));
            }
        });
        Self {
            port,
            targets,
            task,
        }
    }

    /// `socks5h://127.0.0.1:port`; the `h` lets the proxy resolve names.
    pub fn url(&self, remote_dns: bool) -> String {
        let scheme = if remote_dns { "socks5h" } else { "socks5" };
        format!("{scheme}://127.0.0.1:{}", self.port)
    }

    /// Every `host:port` asked for, as the client sent it.
    pub fn targets(&self) -> Vec<String> {
        self.targets.lock().unwrap().clone()
    }
}

impl Drop for SocksProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay(mut client: TcpStream, seen: Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
    // Greeting: version, method count, methods. We take "no login".
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    let mut methods = vec![0u8; usize::from(head[1])];
    client.read_exact(&mut methods).await?;
    client.write_all(&[5, 0]).await?;

    // Request: version, CONNECT, reserved, address type, address, port.
    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    let host = match request[3] {
        1 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let len = client.read_u8().await?;
            let mut name = vec![0u8; usize::from(len)];
            client.read_exact(&mut name).await?;
            String::from_utf8_lossy(&name).into_owned()
        }
        _ => return Ok(()),
    };
    let port = client.read_u16().await?;
    seen.lock().unwrap().push(format!("{host}:{port}"));
    let Ok(mut target) = TcpStream::connect((host.as_str(), port)).await else {
        client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    };
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut target).await?;
    Ok(())
}
