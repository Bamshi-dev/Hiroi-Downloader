//! Serves one generated file for benchmarks:
//!
//! ```text
//! hiroi-testserver [--size 256M] [--rate 10M] [--max-connections N]
//! ```
//!
//! `--rate` limits each connection, the way many real servers do; that
//! is where several connections pay off. The first line printed is the
//! URL, the second the file's SHA-256, so a script can check the copy.

use std::time::Duration;

use hiroi_testserver::{Behavior, TestServer, content};

fn size(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(
        text.find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(text.len()),
    );
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return None,
    };
    number.parse::<u64>().ok().map(|n| n * multiplier)
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mut length = 256 << 20;
    let mut behavior = Behavior::default();
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_default();
        let parsed = size(&value).unwrap_or_else(|| panic!("`{value}` is not a size"));
        match flag.as_str() {
            "--size" => length = parsed,
            "--rate" => behavior.bytes_per_sec = Some(parsed),
            "--max-connections" => behavior.max_connections = Some(parsed as usize),
            other => panic!("unknown option `{other}`"),
        }
    }
    let data = content(usize::try_from(length).expect("size fits in memory"), 42);
    let digest = sha256(&data);
    let server = TestServer::start(behavior).await;
    server.put("bench.bin", data);
    println!("{}", server.url("bench.bin"));
    println!("{digest}");
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}
