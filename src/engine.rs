//! Orquestación de la descarga: reparto dinámico de fragmentos, reintentos,
//! checkpoints de reanudación y publicación atómica del archivo final.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::error::{Error, Result};
use crate::net::{self, HttpClient, Remote};
use crate::plan::{self, Chunk, State};
use crate::proxy::ProxyMode;

pub const DEFAULT_CONNECTIONS: usize = 8;
pub const DEFAULT_MAX_CHUNK: u64 = 4 << 20;
const DEFAULT_ATTEMPTS: u32 = 4;
/// Cada cuántos fragmentos completados se sincroniza el archivo y se persiste el estado.
const FLUSH_EVERY: usize = 16;
const HASH_BLOCK: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Options {
    pub url: String,
    pub output: PathBuf,
    pub connections: usize,
    pub max_chunk: u64,
    pub max_attempts: u32,
    /// SHA-256 esperado en hexadecimal minúsculas. Si se indica, el archivo solo se publica si coincide.
    pub sha256: Option<String>,
    pub proxy: ProxyMode,
    /// Bytes completados, actualizado en tiempo real por las tareas de descarga.
    pub progress: Option<Arc<AtomicU64>>,
}

impl Options {
    pub fn new(url: impl Into<String>, output: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            output: output.into(),
            connections: DEFAULT_CONNECTIONS,
            max_chunk: DEFAULT_MAX_CHUNK,
            max_attempts: DEFAULT_ATTEMPTS,
            sha256: None,
            proxy: ProxyMode::System,
            progress: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Descarga segmentada por rangos, con reanudación.
    Segmented { chunks: usize, connections: usize },
    /// Un solo flujo, sin reanudación. Se usa cuando el servidor no atiende rangos.
    Single,
}

#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub bytes: u64,
    /// Bytes bajados en esta ejecución (excluye lo recuperado de una reanudación).
    pub downloaded: u64,
    pub resumed: u64,
    pub elapsed: Duration,
    pub mode: Mode,
}

pub async fn download(opts: Options) -> Result<Report> {
    let started = Instant::now();
    let client = net::build_client(opts.connections, &opts.proxy)?;
    let remote = net::probe(&client, &opts.url).await?;
    let part = with_suffix(&opts.output, ".part");
    let state_path = with_suffix(&opts.output, ".part.state");

    let transfer = match (remote.ranges, remote.total) {
        (true, Some(total)) if total > 0 => {
            segmented(&client, &opts, &remote, total, &part, &state_path).await?
        }
        _ => single(&client, &opts, &remote, &part).await?,
    };

    if let Some(expected) = &opts.sha256 {
        let actual = sha256_of(&part).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&state_path).await;
            return Err(Error::HashMismatch {
                expected: expected.to_ascii_lowercase(),
                actual,
            });
        }
    }

    tokio::fs::rename(&part, &opts.output).await?;
    remove_if_exists(&state_path).await?;

    Ok(Report {
        bytes: transfer.bytes,
        downloaded: transfer.downloaded,
        resumed: transfer.resumed,
        elapsed: started.elapsed(),
        mode: transfer.mode,
    })
}

struct Transfer {
    bytes: u64,
    downloaded: u64,
    resumed: u64,
    mode: Mode,
}

async fn segmented(
    client: &HttpClient,
    opts: &Options,
    remote: &Remote,
    total: u64,
    part: &Path,
    state_path: &Path,
) -> Result<Transfer> {
    let url = remote.url.to_string();
    let chunk_size = plan::effective_chunk_size(total, opts.connections, opts.max_chunk);

    let resumable = load_state(state_path)
        .await
        .filter(|s| s.matches(&url, total, remote.etag.as_deref()) && s.chunk_size == chunk_size)
        .filter(|_| part.exists());

    let fresh = resumable.is_none();
    let state =
        resumable.unwrap_or_else(|| State::new(&url, total, chunk_size, remote.etag.clone()));
    let file = Arc::new(open_part(part, fresh, total).await?);

    let resumed = state.completed_bytes();
    if let Some(p) = &opts.progress {
        p.fetch_add(resumed, Ordering::Relaxed);
    }

    let pending: Vec<Chunk> = plan::plan(total, chunk_size)
        .into_iter()
        .filter(|c| !state.done[c.index])
        .collect();
    let chunk_count = state.done.len();
    let workers = opts.connections.clamp(1, pending.len().max(1));

    let shared = Arc::new(Shared {
        client: client.clone(),
        url: remote.url.clone(),
        etag: remote.etag.clone(),
        pending,
        cursor: AtomicUsize::new(0),
        unflushed: AtomicUsize::new(0),
        file,
        state: Mutex::new(state),
        state_path: state_path.to_path_buf(),
        progress: opts.progress.clone(),
        max_attempts: opts.max_attempts.max(1),
    });

    let mut set = JoinSet::new();
    for _ in 0..workers {
        let shared = Arc::clone(&shared);
        set.spawn(async move { run_worker(shared).await });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                set.abort_all();
                return Err(e);
            }
            Err(join) => {
                set.abort_all();
                return Err(io::Error::other(join).into());
            }
        }
    }

    checkpoint(&shared).await?;
    let done = shared.state.lock().await.done.iter().all(|d| *d);
    if !done {
        return Err(io::Error::other("quedaron fragmentos sin completar").into());
    }

    Ok(Transfer {
        bytes: total,
        downloaded: total - resumed,
        resumed,
        mode: Mode::Segmented {
            chunks: chunk_count,
            connections: workers,
        },
    })
}

/// Estado compartido por las tareas de descarga de una misma transferencia.
struct Shared {
    client: HttpClient,
    url: hyper::Uri,
    etag: Option<String>,
    pending: Vec<Chunk>,
    /// Siguiente índice de `pending` a reclamar. Reparto dinámico: quien termina antes toma más trabajo.
    cursor: AtomicUsize,
    unflushed: AtomicUsize,
    file: Arc<File>,
    state: Mutex<State>,
    state_path: PathBuf,
    progress: Option<Arc<AtomicU64>>,
    max_attempts: u32,
}

async fn run_worker(shared: Arc<Shared>) -> Result<()> {
    loop {
        let slot = shared.cursor.fetch_add(1, Ordering::Relaxed);
        let Some(chunk) = shared.pending.get(slot).copied() else {
            return Ok(());
        };

        let data = fetch_with_retry(&shared, chunk).await?;
        let file = Arc::clone(&shared.file);
        let offset = chunk.start;
        tokio::task::spawn_blocking(move || write_at(&file, &data, offset))
            .await
            .map_err(io::Error::other)??;

        if let Some(p) = &shared.progress {
            p.fetch_add(chunk.len(), Ordering::Relaxed);
        }

        shared.state.lock().await.done[chunk.index] = true;
        if shared.unflushed.fetch_add(1, Ordering::Relaxed) + 1 >= FLUSH_EVERY {
            shared.unflushed.store(0, Ordering::Relaxed);
            checkpoint(&shared).await?;
        }
    }
}

async fn fetch_with_retry(shared: &Shared, chunk: Chunk) -> Result<Bytes> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match net::fetch_chunk(&shared.client, &shared.url, chunk, shared.etag.as_deref()).await {
            Ok(data) => return Ok(data),
            Err(e) if attempt < shared.max_attempts && is_retryable(&e) => {
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(e) => {
                return Err(Error::ChunkFailed {
                    index: chunk.index,
                    attempts: attempt,
                    source: Box::new(e),
                });
            }
        }
    }
}

/// Persiste estado solo después de sincronizar datos: un fragmento marcado como hecho
/// debe estar realmente en disco. El estado se toma antes de sincronizar, así que todo lo
/// marcado tiene sus bytes escritos antes del `sync_data`.
async fn checkpoint(shared: &Shared) -> Result<()> {
    let snapshot = {
        let state = shared.state.lock().await;
        serde_json::to_vec(&*state)?
    };
    let file = Arc::clone(&shared.file);
    tokio::task::spawn_blocking(move || file.sync_data())
        .await
        .map_err(io::Error::other)??;
    write_state_atomic(&shared.state_path, &snapshot).await
}

async fn single(
    client: &HttpClient,
    opts: &Options,
    remote: &Remote,
    part: &Path,
) -> Result<Transfer> {
    let resp = net::get_full(client, &remote.url).await?;
    let mut body = resp.into_body();
    let mut file = tokio::fs::File::create(part).await?;

    let mut written = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await?;
            written += data.len() as u64;
            if let Some(p) = &opts.progress {
                p.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
    }
    file.sync_all().await?;

    if let Some(total) = remote.total {
        if total != written {
            return Err(Error::Truncated {
                expected: total,
                received: written,
            });
        }
    }

    Ok(Transfer {
        bytes: written,
        downloaded: written,
        resumed: 0,
        mode: Mode::Single,
    })
}

async fn open_part(part: &Path, fresh: bool, total: u64) -> io::Result<File> {
    let part = part.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(fresh)
            .open(&part)?;
        if fresh {
            file.set_len(total)?;
        }
        Ok(file)
    })
    .await
    .map_err(io::Error::other)?
}

fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(data, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut buf = data;
        let mut pos = offset;
        while !buf.is_empty() {
            let n = file.seek_write(buf, pos)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[n..];
            pos += n as u64;
        }
        Ok(())
    }
}

async fn sha256_of(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> io::Result<String> {
        use std::io::Read;
        let mut file = File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; HASH_BLOCK];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    })
    .await
    .map_err(io::Error::other)?
    .map_err(Error::from)
}

async fn load_state(path: &Path) -> Option<State> {
    let bytes = tokio::fs::read(path).await.ok()?;
    // Un estado corrupto no es fatal: se descarta y la descarga empieza de cero.
    serde_json::from_slice(&bytes).ok()
}

async fn write_state_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = with_suffix(path, ".tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

async fn remove_if_exists(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn is_retryable(e: &Error) -> bool {
    match e {
        Error::Connection(_) | Error::Body(_) | Error::Truncated { .. } => true,
        Error::RangeIgnored(s) => *s == 429 || *s >= 500,
        _ => false,
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250u64 << attempt.min(6))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
