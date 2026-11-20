//! Downloads the pieces of a stream (HLS or DASH segments) in parallel
//! and appends them to the output in order.
//!
//! Each piece is streamed to its own small file in a work folder, and
//! decrypted on the fly when the stream uses AES-128. An appender moves
//! finished pieces into the output as soon as all earlier ones are
//! there, so memory stays at a few buffers whatever the stream's length,
//! and disk use at a few pieces beyond the output itself.
//!
//! For a stream that ends (video on demand) the work folder records how
//! many pieces are in the output, so a paused download continues with
//! the next piece. A live stream is recorded until it ends, the time
//! limit is reached, or the user stops it; stopping keeps what was
//! recorded.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use reqwest::header::RANGE;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::constants::{DEFAULT_THROTTLE_PAUSE, WRITE_BUFFER_SIZE};
use crate::error::{EngineError, Result};
use crate::net::host_key;
use crate::net::probe::retry_after;
use crate::net::retry::{FetchError, FetchResult, RetryAction, is_throttle};
use crate::task::{TaskContext, or_cancel};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// One thing to fetch: a segment or an initialization section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piece {
    pub url: Url,
    /// Exclusive byte range within `url`.
    pub range: Option<(u64, u64)>,
    pub key: Option<PieceKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PieceKey {
    pub uri: Url,
    pub iv: [u8; 16],
}

/// Where pieces come from. A finished source has handed out everything;
/// a live one is asked again after `wait`.
pub(crate) struct Batch {
    pub pieces: Vec<Piece>,
    pub finished: bool,
    pub wait: Duration,
}

pub(crate) trait PieceSource {
    async fn next_batch(&mut self) -> Result<Batch>;
}

/// Progress of one track, shared with the reporter.
#[derive(Debug, Default)]
pub(crate) struct TrackProgress {
    pub bytes: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    source: String,
    /// Pieces already in the output, and its length then.
    appended: usize,
    bytes: u64,
}

pub(crate) struct Track<'a> {
    pub ctx: &'a Arc<TaskContext>,
    /// The file the pieces are appended to.
    pub output: PathBuf,
    pub work: PathBuf,
    /// Identifies the stream, so a resume never mixes two streams.
    pub source_key: String,
    pub live: bool,
    /// Stop a live recording after this long.
    pub limit: Option<Duration>,
    pub progress: Arc<TrackProgress>,
}

impl Track<'_> {
    pub async fn run(&self, mut source: impl PieceSource) -> Result<()> {
        let ctx = self.ctx;
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |e| EngineError::io(path, e)
        };
        std::fs::create_dir_all(&self.work).map_err(io(&self.work))?;
        let state_path = self.work.join("state.json");
        let mut state = self.resume_state(&state_path);
        let output = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.output)
            .await
            .map_err(io(&self.output))?;
        // Anything past the recorded length is a piece that was half
        // appended when we stopped.
        output
            .set_len(state.bytes)
            .await
            .map_err(io(&self.output))?;
        let mut output = tokio::io::BufWriter::new(output);
        tokio::io::AsyncSeekExt::seek(&mut output, std::io::SeekFrom::End(0))
            .await
            .map_err(io(&self.output))?;
        self.progress
            .bytes
            .store(state.bytes, std::sync::atomic::Ordering::Relaxed);

        let parallel = ctx
            .request
            .connections
            .unwrap_or(ctx.shared.config.default_connections)
            .clamp(1, 16);
        let started = Instant::now();
        let keys = &tokio::sync::Mutex::new(HashMap::new());
        let mut queue: VecDeque<(usize, Piece)> = VecDeque::new();
        let mut known = 0usize;
        let mut finished = false;
        let mut next_refresh = Instant::now();
        let mut done: BTreeSet<usize> = BTreeSet::new();
        let mut in_flight = FuturesUnordered::new();

        loop {
            let time_up = self.limit.is_some_and(|limit| started.elapsed() >= limit);
            if !finished && !time_up && Instant::now() >= next_refresh {
                let batch = source.next_batch().await?;
                for piece in batch.pieces {
                    // Pieces already in the output (after a resume) are skipped.
                    if known >= state.appended {
                        queue.push_back((known, piece));
                    }
                    known += 1;
                }
                finished = batch.finished;
                next_refresh = Instant::now() + batch.wait;
            }
            if time_up {
                finished = true;
                queue.clear();
            }
            while in_flight.len() < parallel
                && let Some((index, piece)) = queue.pop_front()
            {
                in_flight.push(async move { (index, self.fetch(&piece, index, keys).await) });
            }
            if in_flight.is_empty() && queue.is_empty() && finished {
                break;
            }

            let wait = next_refresh.saturating_duration_since(Instant::now());
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    drop(in_flight);
                    output.flush().await.map_err(io(&self.output))?;
                    // Stopping a live recording keeps what was recorded.
                    return if self.live { Ok(()) } else { Err(EngineError::Cancelled) };
                }
                Some((index, result)) = in_flight.next() => {
                    result?;
                    done.insert(index);
                    while done.remove(&state.appended) {
                        let piece = self.piece_path(state.appended);
                        let mut file = tokio::fs::File::open(&piece).await.map_err(io(&piece))?;
                        state.bytes += tokio::io::copy(&mut file, &mut output).await.map_err(io(&self.output))?;
                        output.flush().await.map_err(io(&self.output))?;
                        let _ = tokio::fs::remove_file(&piece).await;
                        state.appended += 1;
                        if !self.live {
                            save_state(&state_path, &state);
                        }
                    }
                }
                () = tokio::time::sleep(wait), if in_flight.is_empty() && !finished => {}
            }
        }
        output.flush().await.map_err(io(&self.output))?;
        output
            .into_inner()
            .sync_all()
            .await
            .map_err(io(&self.output))?;
        let _ = std::fs::remove_dir_all(&self.work);
        Ok(())
    }

    fn resume_state(&self, path: &Path) -> State {
        let fresh = State {
            source: self.source_key.clone(),
            ..State::default()
        };
        if self.live {
            return fresh;
        }
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok())
            .filter(|state| state.source == self.source_key)
            .unwrap_or(fresh)
    }

    fn piece_path(&self, index: usize) -> PathBuf {
        self.work.join(format!("{index:08}"))
    }

    /// Fetches one piece into its file, retrying as the policy says.
    async fn fetch(
        &self,
        piece: &Piece,
        index: usize,
        keys: &tokio::sync::Mutex<HashMap<Url, [u8; 16]>>,
    ) -> Result<()> {
        let path = self.piece_path(index);
        if path.exists() {
            return Ok(()); // finished before a pause
        }
        let key = match &piece.key {
            Some(key) => Some((self.key(&key.uri, keys).await?, key.iv)),
            None => None,
        };
        let policy = &self.ctx.shared.config.retry;
        let mut attempt = 0;
        loop {
            let error = match self.fetch_once(piece, &path, key).await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            attempt += 1;
            match policy.action(&error, attempt, true) {
                RetryAction::Retry(delay) => {
                    self.ctx
                        .observer
                        .retrying(self.ctx.id, &error.to_string(), delay);
                    or_cancel(&self.ctx.cancel, tokio::time::sleep(delay)).await?;
                }
                _ => return Err(error.into_engine_error(&crate::net::display_url(&piece.url))),
            }
        }
    }

    async fn fetch_once(
        &self,
        piece: &Piece,
        path: &Path,
        key: Option<([u8; 16], [u8; 16])>,
    ) -> FetchResult<()> {
        let ctx = self.ctx;
        let shared = &ctx.shared;
        let _connection = or_cancel(&ctx.cancel, shared.connections.acquire())
            .await?
            .map_err(|_| FetchError::Cancelled)?;
        let gate = shared.hosts.gate(&host_key(&piece.url));
        let _host = or_cancel(&ctx.cancel, gate.acquire()).await?;

        let mut request = ctx
            .client
            .get(&piece.url)?
            .headers(ctx.request.headers.clone());
        if let Some((start, end)) = piece.range {
            request = request.header(RANGE, format!("bytes={start}-{}", end - 1));
        }
        let response = or_cancel(&ctx.cancel, request.send())
            .await?
            .map_err(FetchError::from)?;
        let status = response.status();
        let expected = if piece.range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        if status != expected {
            let retry_after = retry_after(&response);
            if is_throttle(status) {
                gate.throttle(retry_after.unwrap_or(DEFAULT_THROTTLE_PAUSE));
            }
            return Err(FetchError::Status {
                status,
                retry_after,
            });
        }

        let partial = path.with_extension("part");
        let mut file = tokio::fs::File::create(&partial)
            .await
            .map_err(|e| EngineError::io(&partial, e))?;
        let mut decryptor = key.map(|(key, iv)| Decrypt::new(&key, &iv));
        let mut body = response.bytes_stream();
        let mut buffer = Vec::with_capacity(WRITE_BUFFER_SIZE);
        let write_error = |e| FetchError::Fatal(EngineError::io(&partial, e));
        while let Some(chunk) = or_cancel(&ctx.cancel, body.next()).await? {
            let chunk = chunk.map_err(FetchError::from)?;
            or_cancel(&ctx.cancel, async {
                ctx.rate.acquire(chunk.len()).await;
                shared.global_rate.acquire(chunk.len()).await;
            })
            .await?;
            self.progress
                .bytes
                .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
            match &mut decryptor {
                Some(decrypt) => decrypt.update(&chunk, &mut buffer),
                None => buffer.extend_from_slice(&chunk),
            }
            if buffer.len() >= WRITE_BUFFER_SIZE {
                file.write_all(&buffer).await.map_err(write_error)?;
                buffer.clear();
            }
        }
        if let Some(decrypt) = decryptor {
            decrypt.finish(&mut buffer).map_err(FetchError::Protocol)?;
        }
        file.write_all(&buffer).await.map_err(write_error)?;
        file.flush().await.map_err(write_error)?;
        drop(file);
        tokio::fs::rename(&partial, path)
            .await
            .map_err(|e| FetchError::Fatal(EngineError::io(path, e)))?;
        Ok(())
    }

    async fn key(
        &self,
        uri: &Url,
        keys: &tokio::sync::Mutex<HashMap<Url, [u8; 16]>>,
    ) -> Result<[u8; 16]> {
        if let Some(key) = keys.lock().await.get(uri) {
            return Ok(*key);
        }
        let bytes = super::fetch_small(self.ctx, uri, 1024).await?;
        let key: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
            EngineError::Protocol(format!("encryption key is {} bytes, not 16", bytes.len()))
        })?;
        keys.lock().await.insert(uri.clone(), key);
        Ok(key)
    }
}

fn save_state(path: &Path, state: &State) {
    let tmp = path.with_extension("tmp");
    let saved = serde_json::to_vec(state)
        .map(|bytes| std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)));
    if !matches!(saved, Ok(Ok(()))) {
        tracing::warn!("could not save stream progress");
    }
}

/// AES-128-CBC with PKCS#7 padding, one 16-byte block at a time. The last
/// block is held back until the end, because only it carries padding.
pub(crate) struct Decrypt {
    cipher: Aes128CbcDec,
    pending: Vec<u8>,
}

impl Decrypt {
    pub fn new(key: &[u8; 16], iv: &[u8; 16]) -> Self {
        Self {
            cipher: Aes128CbcDec::new(key.into(), iv.into()),
            pending: Vec::with_capacity(32),
        }
    }

    pub fn update(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(data);
        let ready = (self.pending.len().saturating_sub(1) / 16) * 16;
        let blocks: Vec<u8> = self.pending.drain(..ready).collect();
        for block in blocks.chunks_exact(16) {
            out.extend_from_slice(&self.decrypt_block(block));
        }
    }

    pub fn finish(mut self, out: &mut Vec<u8>) -> std::result::Result<(), String> {
        if self.pending.len() != 16 {
            return Err("encrypted segment is not a whole number of blocks".into());
        }
        let block = self.pending.clone();
        let plain = self.decrypt_block(&block);
        let padding = usize::from(plain[15]);
        if padding == 0
            || padding > 16
            || !plain[16 - padding..]
                .iter()
                .all(|&b| usize::from(b) == padding)
        {
            return Err("bad padding: wrong key or damaged segment".into());
        }
        out.extend_from_slice(&plain[..16 - padding]);
        Ok(())
    }

    fn decrypt_block(&mut self, block: &[u8]) -> [u8; 16] {
        let mut block = aes::Block::try_from(block).expect("16-byte block");
        self.cipher.decrypt_block(&mut block);
        block.into()
    }
}

/// The IV HLS uses when a key has none: the sequence number, big-endian.
pub(crate) fn sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockModeEncrypt, block_padding::Pkcs7};

    fn encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let mut buffer = plain.to_vec();
        buffer.resize(plain.len() + 16, 0);
        let length = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
            .encrypt_padded::<Pkcs7>(&mut buffer, plain.len())
            .expect("room for padding")
            .len();
        buffer.truncate(length);
        buffer
    }

    #[test]
    fn decrypts_in_arbitrary_chunks() {
        let (key, iv) = ([7u8; 16], sequence_iv(42));
        let plain: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let cipher = encrypt(&key, &iv, &plain);
        for size in [1, 15, 16, 17, 100, 4096] {
            let mut decrypt = Decrypt::new(&key, &iv);
            let mut out = Vec::new();
            for chunk in cipher.chunks(size) {
                decrypt.update(chunk, &mut out);
            }
            decrypt.finish(&mut out).unwrap();
            assert_eq!(out, plain, "chunk size {size}");
        }
        let mut wrong = Decrypt::new(&[8u8; 16], &iv);
        let mut out = Vec::new();
        wrong.update(&cipher, &mut out);
        assert!(wrong.finish(&mut out).is_err());
    }
}
