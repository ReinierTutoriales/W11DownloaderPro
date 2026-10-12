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
use hyper::Uri;
use hyper::header::HeaderMap;
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
/// Periodo de muestreo para decidir cuántas conexiones usar.
const SAMPLE_EVERY: Duration = Duration::from_millis(250);
/// Ventana mínima de una muestra de velocidad.
const MIN_SAMPLE_WINDOW: Duration = Duration::from_millis(100);

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
    /// Cabeceras adicionales (p. ej. `Cookie`, `Referer`). Solo se envían al host original.
    pub headers: HeaderMap,
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
            headers: HeaderMap::new(),
            progress: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Descarga segmentada por rangos, con reanudación. `connections` es el pico alcanzado y
    /// `settled` las conexiones con las que terminó (difieren si el controlador retrocedió).
    Segmented {
        chunks: usize,
        connections: usize,
        settled: usize,
    },
    /// Un solo flujo, sin reanudación. Se usa cuando el servidor no atiende rangos.
    Single,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    pub bytes: u64,
    /// Bytes bajados en esta ejecución (excluye lo recuperado de una reanudación).
    pub downloaded: u64,
    pub resumed: u64,
    pub elapsed: Duration,
    pub mode: Mode,
}

const RUN: u8 = 0;
const PAUSE: u8 = 1;
const CANCEL: u8 = 2;

/// Control de una descarga en curso. Clonable: el llamador conserva una copia para pausar o cancelar.
#[derive(Debug, Clone, Default)]
pub struct Control(Arc<std::sync::atomic::AtomicU8>);

impl Control {
    pub fn pause(&self) {
        self.0.store(PAUSE, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.0.store(CANCEL, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.0.load(Ordering::SeqCst) == PAUSE
    }

    fn state(&self) -> u8 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Resultado de una descarga controlada.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Completed(Report),
    /// Pausada: el estado de reanudación está guardado. `bytes` son los ya completados.
    Paused {
        bytes: u64,
    },
    Canceled,
}

/// Descarga sin control externo. Si se interrumpiera, devuelve error.
pub async fn download(opts: Options) -> Result<Report> {
    match download_controlled(opts, Control::default()).await? {
        Outcome::Completed(report) => Ok(report),
        Outcome::Paused { .. } | Outcome::Canceled => {
            Err(io::Error::other("descarga interrumpida").into())
        }
    }
}

pub async fn download_controlled(opts: Options, control: Control) -> Result<Outcome> {
    let started = Instant::now();
    let client = net::build_client(opts.connections, &opts.proxy)?;
    let remote = net::probe(&client, &opts.url, &opts.headers).await?;
    let headers = headers_for(&opts.url, &remote.url, &opts.headers);
    let paths = Paths {
        part: with_suffix(&opts.output, ".part"),
        state: with_suffix(&opts.output, ".part.state"),
    };

    let step = match (remote.ranges, remote.total) {
        (true, Some(total)) if total > 0 => {
            segmented(&client, &opts, &remote, &headers, total, &paths, &control).await?
        }
        _ => single(&client, &opts, &remote, &headers, &paths.part, &control).await?,
    };
    let transfer = match step {
        Step::Finished(transfer) => transfer,
        Step::Interrupted { bytes } => {
            return Ok(if control.state() == CANCEL {
                Outcome::Canceled
            } else {
                Outcome::Paused { bytes }
            });
        }
    };

    let (part, state_path) = (&paths.part, &paths.state);
    if let Some(expected) = &opts.sha256 {
        let actual = sha256_of(part).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(part).await;
            let _ = tokio::fs::remove_file(state_path).await;
            return Err(Error::HashMismatch {
                expected: expected.to_ascii_lowercase(),
                actual,
            });
        }
    }

    tokio::fs::rename(part, &opts.output).await?;
    remove_if_exists(state_path).await?;

    Ok(Outcome::Completed(Report {
        bytes: transfer.bytes,
        downloaded: transfer.downloaded,
        resumed: transfer.resumed,
        elapsed: started.elapsed(),
        mode: transfer.mode,
    }))
}

/// Resultado interno de una pasada de descarga: terminada o interrumpida por el control.
enum Step {
    Finished(Transfer),
    Interrupted { bytes: u64 },
}

struct Transfer {
    bytes: u64,
    downloaded: u64,
    resumed: u64,
    mode: Mode,
}

/// Rutas de trabajo de una descarga: el archivo parcial y su estado de reanudación.
struct Paths {
    part: PathBuf,
    state: PathBuf,
}

async fn segmented(
    client: &HttpClient,
    opts: &Options,
    remote: &Remote,
    headers: &HeaderMap,
    total: u64,
    paths: &Paths,
    control: &Control,
) -> Result<Step> {
    let (part, state_path) = (paths.part.as_path(), paths.state.as_path());
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
    // Máximo de conexiones: no más que fragmentos pendientes.
    let max_workers = opts.connections.clamp(1, pending.len().max(1));

    let shared = Arc::new(Shared {
        client: client.clone(),
        control: control.clone(),
        url: remote.url.clone(),
        headers: headers.clone(),
        etag: remote.etag.clone(),
        pending,
        cursor: AtomicUsize::new(0),
        unflushed: AtomicUsize::new(0),
        file,
        state: Mutex::new(state),
        state_path: state_path.to_path_buf(),
        progress: opts.progress.clone(),
        max_attempts: opts.max_attempts.max(1),
        rx_bytes: AtomicU64::new(0),
        limit: AtomicUsize::new(0),
    });

    let mut set = JoinSet::new();
    let mut controller =
        plan::Controller::with_start(max_workers, plan::start_connections(total, max_workers));
    let mut spawned = 0usize;
    let mut peak = controller.conns();
    shared.limit.store(controller.conns(), Ordering::Relaxed);
    spawn_up_to(&mut set, &shared, &mut spawned, controller.conns());

    // Ajuste adaptativo: cada muestra de velocidad total decide el número de conexiones.
    let mut sampler = Sampler::new();
    // Diagnóstico opcional: `W11DL_TRACE=1` imprime cada muestra y la decisión del controlador.
    let trace = std::env::var_os("W11DL_TRACE").is_some();
    let trace_start = Instant::now();
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    // Si el bucle se retrasa, se pierde el tick en vez de recuperarlo en ráfaga con ventanas diminutas.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tokio::select! {
            joined = set.join_next() => {
                match joined {
                    None => break,
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(e))) => {
                        set.abort_all();
                        return Err(e);
                    }
                    Some(Err(join)) => {
                        set.abort_all();
                        return Err(io::Error::other(join).into());
                    }
                }
            }
            _ = tick.tick(), if !controller.settled() => {
                let Some(rate) = sampler.sample(shared.rx_bytes.load(Ordering::Relaxed)) else {
                    continue;
                };
                let conns_before = controller.conns();
                let step = controller.observe(rate);
                if trace {
                    eprintln!(
                        "[trace] t={:.2}s tasa={:.1}MB/s conexiones={} -> {:?}",
                        trace_start.elapsed().as_secs_f64(),
                        rate / 1e6,
                        conns_before,
                        step
                    );
                }
                if let plan::Step::Set(n) = step {
                    shared.limit.store(n, Ordering::Relaxed);
                    spawn_up_to(&mut set, &shared, &mut spawned, n);
                    peak = peak.max(n);
                }
            }
        }
    }

    let done_now = shared.state.lock().await.done.iter().all(|d| *d);
    if !done_now {
        // Interrumpida: se conserva el progreso (pausa) o se borra todo (cancelación).
        if control.state() == CANCEL {
            let _ = tokio::fs::remove_file(part).await;
            let _ = tokio::fs::remove_file(state_path).await;
            return Ok(Step::Interrupted { bytes: 0 });
        }
        checkpoint(&shared).await?;
        let bytes = shared.state.lock().await.completed_bytes();
        return Ok(Step::Interrupted { bytes });
    }
    checkpoint(&shared).await?;

    Ok(Step::Finished(Transfer {
        bytes: total,
        downloaded: total - resumed,
        resumed,
        mode: Mode::Segmented {
            chunks: chunk_count,
            connections: peak,
            settled: controller.conns(),
        },
    }))
}

/// Estado compartido por las tareas de descarga de una misma transferencia.
struct Shared {
    client: HttpClient,
    control: Control,
    url: Uri,
    headers: HeaderMap,
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
    /// Bytes recibidos de la red (continuo, al llegar cada bloque). Alimenta la medición de velocidad
    /// del controlador; no es el progreso confirmado, que cuenta fragmentos completados.
    rx_bytes: AtomicU64,
    /// Número de trabajadores que deben seguir activos. Lo fija el controlador.
    limit: AtomicUsize,
}

/// Lanza trabajadores hasta tener `target`. Los que quedan por encima del límite salen solos.
fn spawn_up_to(
    set: &mut JoinSet<Result<()>>,
    shared: &Arc<Shared>,
    spawned: &mut usize,
    target: usize,
) {
    while *spawned < target {
        let id = *spawned;
        let shared = Arc::clone(shared);
        set.spawn(async move { run_worker(shared, id).await });
        *spawned += 1;
    }
}

/// Velocidad total medida entre dos muestras. Ignora intervalos sin progreso.
struct Sampler {
    last_bytes: u64,
    last_at: Instant,
}

impl Sampler {
    fn new() -> Self {
        Self {
            last_bytes: 0,
            last_at: Instant::now(),
        }
    }

    fn sample(&mut self, bytes: u64) -> Option<f64> {
        self.sample_at(bytes, Instant::now())
    }

    /// Velocidad entre la muestra anterior y `now`. Ignora ventanas sin datos o demasiado cortas:
    /// una ventana de milisegundos (ticks atrasados) daría una velocidad absurda que el controlador
    /// tomaría como referencia. Si se ignora, la ventana sigue acumulando hasta la siguiente muestra.
    fn sample_at(&mut self, bytes: u64, now: Instant) -> Option<f64> {
        let elapsed = now.duration_since(self.last_at);
        if bytes == self.last_bytes || elapsed < MIN_SAMPLE_WINDOW {
            return None;
        }
        let rate = (bytes - self.last_bytes) as f64 / elapsed.as_secs_f64();
        self.last_bytes = bytes;
        self.last_at = now;
        Some(rate)
    }
}

async fn run_worker(shared: Arc<Shared>, id: usize) -> Result<()> {
    loop {
        // Si el controlador redujo conexiones, este trabajador termina tras su fragmento actual.
        // Una pausa o cancelación también termina el trabajador tras el fragmento en curso.
        if id >= shared.limit.load(Ordering::Relaxed) || shared.control.state() != RUN {
            return Ok(());
        }
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
        match net::fetch_chunk(
            &shared.client,
            &shared.url,
            chunk,
            shared.etag.as_deref(),
            &shared.headers,
            &shared.rx_bytes,
        )
        .await
        {
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
    headers: &HeaderMap,
    part: &Path,
    control: &Control,
) -> Result<Step> {
    let resp = net::get_full(client, &remote.url, headers).await?;
    let mut body = resp.into_body();
    let mut file = tokio::fs::File::create(part).await?;

    let mut written = 0u64;
    while let Some(frame) = body.frame().await {
        if control.state() != RUN {
            // Sin rangos no hay forma de reanudar: se descarta lo recibido y la próxima vez empieza de cero.
            drop(file);
            let _ = tokio::fs::remove_file(part).await;
            let bytes = if control.state() == CANCEL {
                0
            } else {
                written
            };
            return Ok(Step::Interrupted { bytes });
        }
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

    Ok(Step::Finished(Transfer {
        bytes: written,
        downloaded: written,
        resumed: 0,
        mode: Mode::Single,
    }))
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

/// Las cabeceras de sesión solo viajan al host original: una redirección a otro dominio no las recibe.
fn headers_for(original: &str, final_url: &Uri, headers: &HeaderMap) -> HeaderMap {
    let same_host = net::parse_url(original)
        .ok()
        .is_some_and(|u| u.authority() == final_url.authority());
    if same_host {
        headers.clone()
    } else {
        HeaderMap::new()
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_muestreador_calcula_la_velocidad_de_la_ventana() {
        let mut s = Sampler::new();
        let t0 = s.last_at;
        let rate = s
            .sample_at(2_000_000, t0 + Duration::from_millis(250))
            .unwrap();
        assert!((rate - 8.0e6).abs() < 1.0, "{rate}");
    }

    #[test]
    fn ventanas_diminutas_se_ignoran_y_se_acumulan() {
        let mut s = Sampler::new();
        let t0 = s.last_at;
        // Ticks atrasados en ráfaga: 5 ms no deben dar una velocidad absurda.
        assert_eq!(s.sample_at(1_000_000, t0 + Duration::from_millis(5)), None);
        // La siguiente muestra suma todo lo acumulado desde la última válida.
        let rate = s
            .sample_at(2_000_000, t0 + Duration::from_millis(250))
            .unwrap();
        assert!((rate - 8.0e6).abs() < 1.0, "{rate}");
    }

    #[test]
    fn sin_bytes_nuevos_no_hay_muestra() {
        let mut s = Sampler::new();
        let t0 = s.last_at;
        assert_eq!(s.sample_at(0, t0 + Duration::from_millis(300)), None);
    }
}
