//! Protocolo de mensajería nativa (Native Messaging) con el navegador.
//!
//! Cada mensaje es JSON precedido de su longitud en 4 bytes little-endian. El host acusa recibo
//! (`accepted`) antes de empezar la descarga: el navegador solo debe cancelar su propia descarga
//! cuando recibe ese acuse, así una descarga nunca se pierde por un fallo de la conexión.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};
use url::Url;

use crate::engine::{self, Control, Options, Outcome};
use crate::limits::RateLimiter;
use crate::naming;
use crate::proxy::ProxyMode;
use crate::queue::{Record, Registry, State};

/// Límite de un mensaje entrante, según el protocolo del navegador.
pub const MAX_INCOMING: usize = 1024 * 1024;
const MAX_CONCURRENT: usize = 4;
const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// Cabeceras que la extensión puede enviar. Cualquier otra se rechaza.
const ALLOWED_HEADERS: &[&str] = &["cookie", "referer", "user-agent", "accept-language"];

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Incoming {
    Ping,
    Enqueue {
        id: String,
        url: String,
        /// Nombre sugerido por el navegador. Si falta, se deriva de la URL.
        filename: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Página o enlace de vídeo: el host lo resuelve con yt-dlp antes de descargar.
    EnqueueVideo {
        id: String,
        page: String,
    },
    /// Pausa una descarga en curso. Conserva el progreso para reanudarla.
    Pause {
        id: String,
    },
    /// Reanuda una descarga pausada, interrumpida o fallida desde su progreso guardado.
    Resume {
        id: String,
    },
    /// Cancela una descarga y borra lo descargado.
    Cancel {
        id: String,
    },
    /// Devuelve la cola completa.
    List,
    /// Fija el límite de velocidad total en bytes/s (0 = sin límite) y lo recuerda. Sin valor,
    /// solo consulta el actual. Se aplica en caliente a las descargas en curso.
    SetLimit {
        bytes_per_sec: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Pong,
    /// La resolución con yt-dlp está en curso. No es final: la petición sigue viva.
    Probing {
        id: String,
    },
    Accepted {
        id: String,
        path: String,
    },
    Progress {
        id: String,
        bytes: u64,
    },
    Done {
        id: String,
        path: String,
        bytes: u64,
    },
    Error {
        id: String,
        message: String,
    },
    Paused {
        id: String,
        bytes: u64,
    },
    Canceled {
        id: String,
    },
    Queue {
        items: Vec<Record>,
    },
    /// Límite de velocidad vigente en bytes/s (0 = sin límite).
    Limit {
        bytes_per_sec: u64,
    },
}

/// Lee un mensaje. `Ok(None)` indica que el navegador cerró la conexión.
pub async fn read_message<R: AsyncRead + Unpin>(input: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_bytes = [0u8; 4];
    match input.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_INCOMING {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mensaje demasiado grande",
        ));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).await?;
    Ok(Some(body))
}

pub async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    output: &mut W,
    value: &T,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(std::io::Error::other)?;
    output.write_all(&len.to_le_bytes()).await?;
    output.write_all(&body).await?;
    output.flush().await
}

/// Atiende mensajes hasta que el navegador cierra la conexión. Las descargas continúan hasta
/// terminar; el proceso espera a que terminen antes de salir.
pub async fn serve<R, W>(mut input: R, mut output: W, out_dir: PathBuf) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            write_message(&mut output, &message).await?;
        }
        Ok::<_, std::io::Error>(())
    });

    let slots = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let limiter = RateLimiter::new(settings::load_limit(&out_dir));
    let mut jobs = tokio::task::JoinSet::new();
    let registry = Registry::load(&out_dir);
    if let Err(e) = registry.save(&out_dir) {
        eprintln!("w11dl-host: no se pudo guardar la cola: {e}");
    }

    while let Some(raw) = read_message(&mut input).await? {
        match serde_json::from_slice::<Incoming>(&raw) {
            Ok(Incoming::Ping) => {
                let _ = tx.send(Outgoing::Pong);
            }
            Ok(Incoming::EnqueueVideo { id, page }) => {
                if !is_web(&page) {
                    let _ = tx.send(Outgoing::Error {
                        id,
                        message: format!("página no web: {page}"),
                    });
                    continue;
                }
                let _ = tx.send(Outgoing::Probing { id: id.clone() });
                let slots = Arc::clone(&slots);
                let tx = tx.clone();
                let out_dir = out_dir.clone();
                let registry = registry.clone();
                let limiter = Arc::clone(&limiter);
                jobs.spawn(async move {
                    let _permit = slots.acquire_owned().await.expect("semáforo abierto");
                    run_video(id, page, out_dir, registry, tx, limiter).await;
                });
            }
            Ok(Incoming::Enqueue {
                id,
                url,
                filename,
                headers,
            }) => match prepare(&id, &url, filename.as_deref(), &headers, &out_dir) {
                Ok(mut job) => {
                    let _ = tx.send(Outgoing::Accepted {
                        id: id.clone(),
                        path: job.output.display().to_string(),
                    });
                    job.control = registry.insert(
                        Record {
                            id: id.clone(),
                            url: job.url.clone(),
                            path: job.output.clone(),
                            state: State::Running,
                            bytes: 0,
                        },
                        job.headers.clone(),
                    );
                    let _ = registry.save(&out_dir);
                    let slots = Arc::clone(&slots);
                    let tx = tx.clone();
                    let registry = registry.clone();
                    let out_dir = out_dir.clone();
                    let limiter = Arc::clone(&limiter);
                    jobs.spawn(async move {
                        let _permit = slots.acquire_owned().await.expect("semáforo abierto");
                        run(job, tx, registry, out_dir, limiter).await;
                    });
                }
                Err(message) => {
                    let _ = tx.send(Outgoing::Error { id, message });
                }
            },
            Ok(Incoming::Pause { id }) => {
                let running = registry
                    .get(&id)
                    .filter(|e| e.record.state == State::Running);
                match running {
                    Some(entry) => entry.control.pause(),
                    None => {
                        let _ = tx.send(Outgoing::Error {
                            id,
                            message: "no está en curso: solo se pausan descargas activas".into(),
                        });
                    }
                }
            }
            Ok(Incoming::Cancel { id }) => match registry.get(&id) {
                Some(entry) if entry.record.state == State::Running => entry.control.cancel(),
                Some(entry) if entry.record.state.is_resumable() => {
                    remove_partials(&entry.record.path).await;
                    registry.update(&id, |e| e.record.state = State::Canceled);
                    let _ = registry.save(&out_dir);
                    let _ = tx.send(Outgoing::Canceled { id });
                }
                _ => {
                    let _ = tx.send(Outgoing::Error {
                        id,
                        message: "no hay nada que cancelar".into(),
                    });
                }
            },
            Ok(Incoming::Resume { id }) => match registry.get(&id) {
                Some(entry) if entry.record.state.is_resumable() => {
                    let control = Control::default();
                    registry.update(&id, |e| {
                        e.control = control.clone();
                        e.record.state = State::Running;
                    });
                    let _ = registry.save(&out_dir);
                    let job = Job {
                        id: id.clone(),
                        url: entry.record.url.clone(),
                        output: entry.record.path.clone(),
                        headers: entry.headers.clone(),
                        control,
                    };
                    let _ = tx.send(Outgoing::Accepted {
                        id: id.clone(),
                        path: entry.record.path.display().to_string(),
                    });
                    let slots = Arc::clone(&slots);
                    let tx = tx.clone();
                    let registry = registry.clone();
                    let out_dir = out_dir.clone();
                    let limiter = Arc::clone(&limiter);
                    jobs.spawn(async move {
                        let _permit = slots.acquire_owned().await.expect("semáforo abierto");
                        run(job, tx, registry, out_dir, limiter).await;
                    });
                }
                _ => {
                    let _ = tx.send(Outgoing::Error {
                        id,
                        message: "esta descarga no se puede reanudar".into(),
                    });
                }
            },
            Ok(Incoming::SetLimit { bytes_per_sec }) => {
                if let Some(rate) = bytes_per_sec {
                    limiter.set_rate(rate);
                    if let Err(e) = settings::save_limit(&out_dir, rate) {
                        eprintln!("w11dl-host: no se pudo guardar el límite: {e}");
                    }
                }
                let _ = tx.send(Outgoing::Limit {
                    bytes_per_sec: limiter.rate(),
                });
            }
            Ok(Incoming::List) => {
                let _ = tx.send(Outgoing::Queue {
                    items: registry.records(),
                });
            }
            Err(e) => {
                let _ = tx.send(Outgoing::Error {
                    id: String::new(),
                    message: format!("mensaje no válido: {e}"),
                });
            }
        }
    }

    while jobs.join_next().await.is_some() {}
    drop(tx);
    writer.await.map_err(std::io::Error::other)??;
    Ok(())
}

#[derive(Debug)]
struct Job {
    id: String,
    url: String,
    output: PathBuf,
    headers: HeaderMap,
    control: Control,
}

/// Valida la petición antes de aceptarla: URL web, cabeceras permitidas y nombre seguro.
fn prepare(
    id: &str,
    url: &str,
    filename: Option<&str>,
    headers: &HashMap<String, String>,
    out_dir: &std::path::Path,
) -> Result<Job, String> {
    let parsed = Url::parse(url).map_err(|_| format!("URL no válida: {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("esquema no permitido: {}", parsed.scheme()));
    }

    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if !ALLOWED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(format!("cabecera no permitida: {name}"));
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|e| e.to_string())?;
        map.append(name, value);
    }

    let name = filename
        .and_then(naming::sanitize)
        .unwrap_or_else(|| naming::file_name(&parsed));
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let output = naming::unique_path(out_dir, &name);

    Ok(Job {
        id: id.to_owned(),
        url: url.to_owned(),
        output,
        headers: map,
        control: Control::default(),
    })
}

fn is_web(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Resuelve la página con yt-dlp y, si hay un formato, la descarga como una petición normal.
async fn run_video(
    id: String,
    page: String,
    out_dir: PathBuf,
    registry: Registry,
    tx: mpsc::UnboundedSender<Outgoing>,
    limiter: Arc<RateLimiter>,
) {
    let opts = crate::video::ProbeOptions {
        python: crate::video::default_python(),
        cookies: None,
    };
    let choice = match crate::video::probe(&page, &opts).await {
        Ok(choice) => choice,
        Err(e) => {
            let _ = tx.send(Outgoing::Error {
                id,
                message: e.to_string(),
            });
            return;
        }
    };
    let output = naming::unique_path(&out_dir, &choice.filename);
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        let _ = tx.send(Outgoing::Error {
            id,
            message: e.to_string(),
        });
        return;
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &choice.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
    let _ = tx.send(Outgoing::Accepted {
        id: id.clone(),
        path: output.display().to_string(),
    });
    let control = registry.insert(
        Record {
            id: id.clone(),
            url: choice.url.clone(),
            path: output.clone(),
            state: State::Running,
            bytes: 0,
        },
        headers.clone(),
    );
    let job = Job {
        id,
        url: choice.url,
        output,
        headers,
        control,
    };
    run(job, tx, registry, out_dir, limiter).await;
}

/// Ejecuta una descarga controlada y deja su resultado en la cola (y en disco).
async fn run(
    job: Job,
    tx: mpsc::UnboundedSender<Outgoing>,
    registry: Registry,
    out_dir: PathBuf,
    limiter: Arc<RateLimiter>,
) {
    let progress = Arc::new(AtomicU64::new(0));
    let reporter = {
        let progress = Arc::clone(&progress);
        let tx = tx.clone();
        let id = job.id.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PROGRESS_EVERY);
            loop {
                ticker.tick().await;
                let bytes = progress.load(Ordering::Relaxed);
                let _ = tx.send(Outgoing::Progress {
                    id: id.clone(),
                    bytes,
                });
            }
        })
    };

    let mut opts = Options::new(job.url, job.output.clone());
    opts.headers = job.headers;
    opts.proxy = ProxyMode::System;
    opts.progress = Some(Arc::clone(&progress));
    opts.limiter = Some(limiter);

    let result = engine::download_controlled(opts, job.control).await;
    reporter.abort();

    let message = match result {
        Ok(Outcome::Completed(report)) => {
            registry.update(&job.id, |e| {
                e.record.state = State::Done;
                e.record.bytes = report.bytes;
            });
            Outgoing::Done {
                id: job.id.clone(),
                path: job.output.display().to_string(),
                bytes: report.bytes,
            }
        }
        Ok(Outcome::Paused { bytes }) => {
            registry.update(&job.id, |e| {
                e.record.state = State::Paused;
                e.record.bytes = bytes;
            });
            Outgoing::Paused {
                id: job.id.clone(),
                bytes,
            }
        }
        Ok(Outcome::Canceled) => {
            registry.update(&job.id, |e| e.record.state = State::Canceled);
            Outgoing::Canceled { id: job.id.clone() }
        }
        Err(e) => {
            let message = e.to_string();
            registry.update(&job.id, |en| {
                en.record.state = State::Failed {
                    message: message.clone(),
                }
            });
            Outgoing::Error {
                id: job.id.clone(),
                message,
            }
        }
    };
    if let Err(e) = registry.save(&out_dir) {
        eprintln!("w11dl-host: no se pudo guardar la cola: {e}");
    }
    let _ = tx.send(message);
}

/// Ajustes persistentes del host (solo el límite de velocidad; nada de sesión).
mod settings {
    use std::path::Path;

    const FILE: &str = ".w11dl-settings.json";

    pub fn load_limit(dir: &Path) -> u64 {
        std::fs::read_to_string(dir.join(FILE))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("limit_bytes_per_sec")?.as_u64())
            .unwrap_or(0)
    }

    pub fn save_limit(dir: &Path, rate: u64) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(
            &tmp,
            serde_json::json!({ "limit_bytes_per_sec": rate }).to_string(),
        )?;
        std::fs::rename(&tmp, dir.join(FILE))
    }
}

/// Borra el parcial y su estado de reanudación de una descarga cancelada.
async fn remove_partials(output: &Path) {
    let part = naming_with_suffix(output, ".part");
    let state = naming_with_suffix(output, ".part.state");
    let _ = tokio::fs::remove_file(part).await;
    let _ = tokio::fs::remove_file(state).await;
}

fn naming_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn marco_ida_y_vuelta() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_message(&mut a, &Outgoing::Pong).await.unwrap();
        let raw = read_message(&mut b).await.unwrap().unwrap();
        assert_eq!(raw, br#"{"type":"pong"}"#);
    }

    #[tokio::test]
    async fn mensaje_demasiado_grande_se_rechaza() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&((MAX_INCOMING as u32) + 1).to_le_bytes())
            .await
            .unwrap();
        assert!(read_message(&mut b).await.is_err());
    }

    #[test]
    fn cabecera_no_permitida_se_rechaza() {
        let dir = tempfile::tempdir().unwrap();
        let headers = HashMap::from([("X-Evil".to_owned(), "1".to_owned())]);
        let err = prepare("1", "https://x.test/a.zip", None, &headers, dir.path()).unwrap_err();
        assert!(err.contains("no permitida"), "{err}");
    }

    #[test]
    fn esquema_no_web_se_rechaza() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            prepare("1", "file:///etc/passwd", None, &HashMap::new(), dir.path()).unwrap_err();
        assert!(err.contains("esquema"), "{err}");
    }

    #[test]
    fn nombre_sugerido_no_escapa_del_directorio() {
        let dir = tempfile::tempdir().unwrap();
        let job = prepare(
            "1",
            "https://x.test/a.zip",
            Some("..\\..\\evil.exe"),
            &HashMap::new(),
            dir.path(),
        )
        .unwrap();
        assert_eq!(job.output.parent().unwrap(), dir.path());
        assert_eq!(job.output.file_name().unwrap(), "evil.exe");
    }
}
