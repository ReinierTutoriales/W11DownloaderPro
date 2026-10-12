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
pub async fn download_merged(base: Options, job: MergeJob, control: Control) -> Result<Outcome> {
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
            for file in [&video_file, &audio_file] {
                remove_track(file).await;
            }
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
            let args = ffmpeg_args(&video_file, &audio_file, &tmp, container(&output));
            let result = Command::new(&job.ffmpeg)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| Error::Video(format!("no se pudo ejecutar ffmpeg: {e}")))?;
            if !result.status.success() {
                let _ = tokio::fs::remove_file(&tmp).await;
                let stderr = String::from_utf8_lossy(&result.stderr);
                let line = stderr.lines().rev().find(|l| !l.trim().is_empty());
                // Las pistas se conservan: reintentar no vuelve a descargarlas.
                return Err(Error::Video(format!(
                    "ffmpeg no pudo unir las pistas: {}",
                    line.unwrap_or("error desconocido").trim()
                )));
            }
            tokio::fs::rename(&tmp, &output).await?;
            let bytes = tokio::fs::metadata(&output).await?.len();
            for file in [&video_file, &audio_file] {
                let _ = tokio::fs::remove_file(file).await;
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

/// Descarga una pista; si ya está completa de una ejecución anterior, no la repite.
async fn fetch_track(opts: Options, control: Control) -> Result<Outcome> {
    if let Ok(meta) = tokio::fs::metadata(&opts.output).await {
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
    engine::download_controlled(opts, control).await
}

async fn remove_track(file: &Path) {
    let _ = tokio::fs::remove_file(file).await;
    let _ = tokio::fs::remove_file(with_suffix(file, ".part")).await;
    let _ = tokio::fs::remove_file(with_suffix(file, ".part.state")).await;
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
