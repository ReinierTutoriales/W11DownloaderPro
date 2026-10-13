//! Descarga de imagen y sonido por separado y unión con ffmpeg sin recodificar.
//!
//! Cada pista se descarga con el motor completo (segmentada, reanudable y con límite) a un archivo
//! intermedio estable. Si la descarga se pausa y se reanuda, la pista ya terminada no se vuelve a
//! bajar. El archivo final solo aparece cuando la unión terminó bien.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use hyper::header::HeaderMap;
use tokio::process::Command;

use crate::engine::{self, Control, Options, Outcome, Report};
use crate::error::{Error, Result};

/// Una pista a descargar.
#[derive(Debug, Clone)]
pub struct Track {
    pub url: String,
    pub headers: HeaderMap,
}

/// Lo necesario para unir: las dos pistas y dónde está ffmpeg.
#[derive(Debug, Clone)]
pub struct MergeJob {
    pub video: Track,
    pub audio: Track,
    pub ffmpeg: PathBuf,
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Formato de contenedor que ffmpeg debe escribir según la extensión final.
fn container(output: &Path) -> &'static str {
    match output
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("webm") => "webm",
        Some("mkv") => "matroska",
        Some("mov") => "mov",
        _ => "mp4",
    }
}

/// Argumentos de ffmpeg: copia las dos pistas tal cual al contenedor de salida.
fn ffmpeg_args(video: &Path, audio: &Path, tmp: &Path, format: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for a in ["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"] {
        args.push(a.into());
    }
    args.push(video.into());
    args.push("-i".into());
    args.push(audio.into());
    for a in ["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"] {
        args.push(a.into());
    }
    if format == "mp4" {
        // Índice al principio: el archivo se reproduce sin esperar a leerlo entero.
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    args.push("-f".into());
    args.push(format.into());
    args.push(tmp.into());
    args
}

/// Descarga ambas pistas en paralelo y las une en `base.output`.
/// `base` aporta conexiones, proxy, límite y progreso; su URL y cabeceras se ignoran.
///
/// El destino queda reservado (mismo bloqueo que una descarga directa) desde antes de elegir los
/// intermedios hasta publicar y limpiar: dos trabajos sobre el mismo destino no comparten `.merging`.
pub async fn download_merged(base: Options, job: MergeJob, control: Control) -> Result<Outcome> {
    let _destination = engine::DestinationLock::acquire(&base.output).await?;
    merge_locked(base, job, control).await
}

async fn merge_locked(base: Options, job: MergeJob, control: Control) -> Result<Outcome> {
    let started = Instant::now();
    let output = base.output.clone();
    let video_file = with_suffix(&output, ".video");
    let audio_file = with_suffix(&output, ".audio");
    // Ambas pistas suman en el mismo contador: el progreso es el de la descarga completa.
    let progress = base
        .progress
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicU64::new(0)));

    let part = |track: &Track, file: &Path| {
        let mut o = base.clone();
        o.url = track.url.clone();
        o.headers = track.headers.clone();
        o.output = file.to_path_buf();
        o.sha256 = None;
        #[cfg(feature = "test-hooks")]
        {
            o.gate = None;
        }
        o.progress = Some(Arc::clone(&progress));
        o
    };
    let (video_opts, audio_opts) = (part(&job.video, &video_file), part(&job.audio, &audio_file));

    let (v, a) = tokio::try_join!(
        fetch_track(video_opts, control.clone()),
        fetch_track(audio_opts, control.clone())
    )?;

    match (v, a) {
        (Outcome::Canceled, _) | (_, Outcome::Canceled) => {
            discard_tracks(&output).await?;
            Ok(Outcome::Canceled)
        }
        (Outcome::Paused { bytes: x }, Outcome::Paused { bytes: y }) => {
            Ok(Outcome::Paused { bytes: x + y })
        }
        (Outcome::Paused { bytes }, Outcome::Completed(r))
        | (Outcome::Completed(r), Outcome::Paused { bytes }) => Ok(Outcome::Paused {
            bytes: bytes + r.bytes,
        }),
        (Outcome::Completed(rv), Outcome::Completed(ra)) => {
            let tmp = with_suffix(&output, ".merging");
            match run_ffmpeg(&job, &video_file, &audio_file, &tmp, &output, &control).await? {
                Joined::Interrupted => {
                    return stop(&control, &output, &tmp, rv.bytes + ra.bytes).await;
                }
                Joined::Failed(message) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    // Las pistas se conservan: reintentar no vuelve a descargarlas.
                    return Err(Error::Video(message));
                }
                Joined::Done => {}
            }
            #[cfg(feature = "test-hooks")]
            if let Some(gate) = &base.gate {
                gate.reached.add_permits(1);
                if let Ok(permit) = gate.release.acquire().await {
                    permit.forget();
                }
            }
            // Una pausa o cancelación que llegó justo al terminar la unión también se respeta antes de publicar.
            if control.is_interrupted() {
                return stop(&control, &output, &tmp, rv.bytes + ra.bytes).await;
            }
            tokio::fs::rename(&tmp, &output).await?;
            let bytes = tokio::fs::metadata(&output).await?.len();
            for file in [&video_file, &audio_file] {
                let _ = tokio::fs::remove_file(file).await;
                let _ = tokio::fs::remove_file(manifest_of(file)).await;
            }
            progress.store(bytes, Ordering::Relaxed);
            Ok(Outcome::Completed(Report {
                bytes,
                downloaded: rv.downloaded + ra.downloaded,
                resumed: rv.resumed + ra.resumed,
                elapsed: started.elapsed(),
                mode: rv.mode,
            }))
        }
    }
}

/// Fin anticipado por pausa o cancelación, sin publicar nada: el temporal se retira siempre; pausar
/// conserva las pistas y cancelar las borra.
async fn stop(control: &Control, output: &Path, tmp: &Path, bytes: u64) -> Result<Outcome> {
    engine::remove_if_exists(tmp).await?;
    if control.is_canceled() {
        discard_tracks(output).await?;
        Ok(Outcome::Canceled)
    } else {
        Ok(Outcome::Paused { bytes })
    }
}

enum Joined {
    Done,
    Failed(String),
    Interrupted,
}

/// Ejecuta ffmpeg atendiendo el control: pausar o cancelar lo termina y lo espera antes de seguir.
async fn run_ffmpeg(
    job: &MergeJob,
    video: &Path,
    audio: &Path,
    tmp: &Path,
    output: &Path,
    control: &Control,
) -> Result<Joined> {
    let args = ffmpeg_args(video, audio, tmp, container(output));
    let mut child = Command::new(&job.ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Video(format!("no se pudo ejecutar ffmpeg: {e}")))?;
    let mut stderr = child.stderr.take();
    let collect = tokio::spawn(async move {
        let mut text = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            use tokio::io::AsyncReadExt;
            let _ = pipe.take(64 * 1024).read_to_end(&mut text).await;
        }
        text
    });
    let status = tokio::select! {
        status = child.wait() => status.map_err(|e| Error::Video(format!("ffmpeg: {e}")))?,
        _ = control.interrupted() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            collect.abort();
            return Ok(Joined::Interrupted);
        }
    };
    let stderr = collect.await.unwrap_or_default();
    if status.success() {
        return Ok(Joined::Done);
    }
    let stderr = String::from_utf8_lossy(&stderr);
    let line = stderr.lines().rev().find(|l| !l.trim().is_empty());
    Ok(Joined::Failed(format!(
        "ffmpeg no pudo unir las pistas: {}",
        line.unwrap_or("error desconocido").trim()
    )))
}

/// Manifiesto de una pista completa: la identidad del recurso del que salió.
fn manifest_of(track: &Path) -> PathBuf {
    with_suffix(track, ".done")
}

async fn write_manifest(track: &Path, identity: &engine::Identity) -> Result<()> {
    let target = manifest_of(track);
    let tmp = with_suffix(&target, ".tmp");
    let json = serde_json::to_vec(identity).map_err(std::io::Error::other)?;
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, &target).await?;
    Ok(())
}

async fn read_manifest(track: &Path) -> Option<engine::Identity> {
    let text = tokio::fs::read(manifest_of(track)).await.ok()?;
    serde_json::from_slice(&text).ok()
}

/// Descarga una pista. Una pista completa de una ejecución anterior solo se reutiliza si su
/// manifiesto coincide con la identidad actual del recurso (mismo tamaño y mismo validador fuerte); si
/// no puede comprobarse, se vuelve a descargar. Reutilizarla a ciegas podría unir pistas de
/// selecciones o contenidos distintos.
async fn fetch_track(opts: Options, control: Control) -> Result<Outcome> {
    if let Ok(meta) = tokio::fs::metadata(&opts.output).await {
        let saved = read_manifest(&opts.output).await;
        let now = engine::identify(&opts).await.ok().flatten();
        let reusable =
            matches!((&saved, &now), (Some(a), Some(b)) if a == b && a.total == meta.len());
        if reusable {
            if let Some(p) = &opts.progress {
                p.fetch_add(meta.len(), Ordering::Relaxed);
            }
            return Ok(Outcome::Completed(Report {
                bytes: meta.len(),
                downloaded: 0,
                resumed: meta.len(),
                elapsed: std::time::Duration::ZERO,
                mode: engine::Mode::Single,
            }));
        }
        engine::remove_if_exists(&opts.output).await?;
    }
    engine::remove_if_exists(&manifest_of(&opts.output)).await?;

    let before = engine::identify(&opts).await.ok().flatten();
    let outcome = engine::download_controlled(opts.clone(), control).await?;
    if let (Outcome::Completed(_), Some(before)) = (&outcome, before) {
        // Solo se certifica si el recurso sigue siendo el mismo después de bajarlo.
        if engine::identify(&opts).await.ok().flatten().as_ref() == Some(&before) {
            write_manifest(&opts.output, &before).await?;
        }
    }
    Ok(outcome)
}

/// Borra las pistas, sus parciales y el temporal de unión de un destino. Ausente no es un fallo; el
/// resto de errores se comunican (no se confirma una cancelación que dejó archivos).
async fn discard_tracks(output: &Path) -> Result<()> {
    let mut first: Option<std::io::Error> = None;
    let video = with_suffix(output, ".video");
    let audio = with_suffix(output, ".audio");
    let files = [
        video.clone(),
        with_suffix(&video, ".part"),
        with_suffix(&video, ".part.state"),
        manifest_of(&video),
        audio.clone(),
        with_suffix(&audio, ".part"),
        with_suffix(&audio, ".part.state"),
        manifest_of(&audio),
        with_suffix(output, ".merging"),
    ];
    for file in files {
        if let Err(e) = engine::remove_if_exists(&file).await {
            first.get_or_insert(e);
        }
    }
    match first {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// Cancela un trabajo compuesto que no está en curso: toma el destino y borra todo lo propio.
pub async fn discard(output: &Path) -> Result<()> {
    let destination = engine::DestinationLock::acquire(output).await?;
    let tracks = discard_tracks(output).await;
    let partials = {
        let state = engine::remove_if_exists(&with_suffix(output, ".part.state")).await;
        state.map_err(Error::from)
    };
    drop(destination);
    tracks?;
    partials
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contenedor_segun_extension() {
        assert_eq!(container(Path::new("a.mp4")), "mp4");
        assert_eq!(container(Path::new("a.WEBM")), "webm");
        assert_eq!(container(Path::new("a.mkv")), "matroska");
        assert_eq!(container(Path::new("a.xyz")), "mp4");
    }

    #[test]
    fn argumentos_copian_sin_recodificar() {
        let args: Vec<String> = ffmpeg_args(Path::new("v"), Path::new("a"), Path::new("t"), "mp4")
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("-c copy"), "{joined}");
        assert!(joined.contains("-map 0:v:0 -map 1:a:0"), "{joined}");
        assert!(joined.contains("+faststart"), "{joined}");
        assert!(joined.contains("-nostdin"), "{joined}");
    }
}
