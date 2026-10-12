//! Descarga delegada en yt-dlp: lo que el motor propio no baja directamente (HLS, DASH, fragmentos,
//! otros contenedores) lo descarga yt-dlp, que cubre los sitios que soporta y sabe unir las pistas.
//! El motor propio sigue encargándose de todo lo que es un archivo directo.
//!
//! Los intermedios viven en una carpeta temporal exclusiva de la descarga: pausar mata el proceso y
//! conserva esa carpeta (yt-dlp retoma los fragmentos al relanzarse, incluso tras reiniciar el
//! host, porque vuelve a resolver el enlace); cancelar la borra entera.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::engine::{Control, Mode, Outcome, Report};
use crate::error::{Error, Result};

/// Fragmentos que yt-dlp baja a la vez en HLS y DASH.
pub const FRAGMENTS: usize = 8;
const POLL: Duration = Duration::from_millis(150);
const MAX_STDERR: usize = 64 * 1024;
const PROGRESS_PREFIX: &str = "W11DL";

#[derive(Debug, Clone)]
pub struct YtDlpJob {
    pub python: PathBuf,
    pub ffmpeg: Option<PathBuf>,
    pub page: String,
    pub out_dir: PathBuf,
    /// Nombre sin extensión: yt-dlp añade la que corresponda al contenedor final.
    pub stem: String,
    /// Carpeta de intermedios, exclusiva de esta descarga.
    pub temp_dir: PathBuf,
    /// Bytes por segundo; 0 = sin límite.
    pub rate_limit: u64,
    pub cookies: Option<PathBuf>,
}

/// Escapa `%` para que yt-dlp no lo interprete como parte de la plantilla de salida.
fn escape_template(text: &str) -> String {
    text.replace('%', "%%")
}

/// Argumentos de yt-dlp. Sin DRM (yt-dlp no descarga contenido protegido por defecto) ni listas.
pub fn build_args(job: &YtDlpJob) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |a: &str| args.push(a.into());
    for a in crate::runtime::ytdlp_prefix(&job.python) {
        push(a);
    }
    for a in [
        "--no-playlist",
        "--no-warnings",
        "--no-cache-dir",
        "--no-mtime",
        "--quiet",
        "--progress",
        "--newline",
        "--progress-template",
    ] {
        push(a);
    }
    push(&format!(
        "download:{PROGRESS_PREFIX} %(info.format_id)s %(progress.downloaded_bytes)s"
    ));
    for a in ["--print", "after_move:filepath", "-N"] {
        push(a);
    }
    push(&FRAGMENTS.to_string());
    push("-f");
    // Con ffmpeg se une la mejor imagen sola con el mejor sonido solo; si el sitio ya entrega un formato
    // completo (o no hay pistas separadas) se usa ese. `bv*` no sirve: elegiría el completo dos veces.
    push(if job.ffmpeg.is_some() { "bv+ba/b" } else { "b" });
    for a in ["--merge-output-format", "mp4/mkv"] {
        push(a);
    }
    if job.rate_limit > 0 {
        push("--limit-rate");
        push(&job.rate_limit.to_string());
    }
    args.push("-P".into());
    args.push(format!("home:{}", job.out_dir.display()).into());
    args.push("-P".into());
    args.push(format!("temp:{}", job.temp_dir.display()).into());
    args.push("-o".into());
    args.push(format!("{}.%(ext)s", escape_template(&job.stem)).into());
    if let Some(ffmpeg) = &job.ffmpeg {
        args.push("--ffmpeg-location".into());
        args.push(ffmpeg.into());
    }
    if let Some(cookies) = &job.cookies {
        args.push("--cookies".into());
        args.push(cookies.into());
    }
    args.push("--".into());
    args.push((&job.page).into());
    args
}

/// Línea de progreso `W11DL <formato> <bytes>`. Ignora valores no numéricos (`NA`).
pub fn parse_progress(line: &str) -> Option<(String, u64)> {
    let mut parts = line.split_whitespace();
    if parts.next()? != PROGRESS_PREFIX {
        return None;
    }
    let format = parts.next()?.to_owned();
    let bytes: f64 = parts.next()?.parse().ok()?;
    (bytes.is_finite() && bytes >= 0.0).then_some((format, bytes as u64))
}

#[derive(Default)]
struct Seen {
    per_format: HashMap<String, u64>,
    final_path: Option<String>,
}

impl Seen {
    fn total(&self) -> u64 {
        self.per_format.values().sum()
    }
}

/// Descarga con yt-dlp. Devuelve el resultado y, si terminó, la ruta real del archivo final
/// (la extensión la decide yt-dlp según el contenedor).
pub async fn download(
    job: &YtDlpJob,
    control: Control,
    progress: Option<Arc<AtomicU64>>,
) -> Result<(Outcome, Option<PathBuf>)> {
    let started = Instant::now();
    tokio::fs::create_dir_all(&job.out_dir).await?;
    let mut child = Command::new(&job.python)
        .args(build_args(job))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Video(format!("no se pudo ejecutar yt-dlp: {e}")))?;

    let seen = Arc::new(Mutex::new(Seen::default()));
    let stdout = child.stdout.take().expect("stdout canalizado");
    let reader = {
        let seen = Arc::clone(&seen);
        let progress = progress.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((format, bytes)) = parse_progress(&line) {
                    seen.per_format.insert(format, bytes);
                    if let Some(p) = &progress {
                        p.store(seen.total(), Ordering::Relaxed);
                    }
                } else if !line.trim().is_empty() {
                    seen.final_path = Some(line.trim().to_owned());
                }
            }
        })
    };
    let mut stderr = child.stderr.take().expect("stderr canalizado");
    let errors = tokio::spawn(async move {
        let mut text = Vec::new();
        let _ = (&mut stderr)
            .take(MAX_STDERR as u64)
            .read_to_end(&mut text)
            .await;
        String::from_utf8_lossy(&text).into_owned()
    });

    // Espera a que termine, atendiendo pausa y cancelación entre medias.
    let status = loop {
        tokio::select! {
            status = child.wait() => break Some(status),
            _ = tokio::time::sleep(POLL) => {
                if control.is_paused() || control.is_canceled() {
                    kill_tree(&mut child).await;
                    break None;
                }
            }
        }
    };
    let _ = reader.await;
    let stderr_text = errors.await.unwrap_or_default();
    let bytes_seen = seen.lock().unwrap_or_else(|e| e.into_inner()).total();

    let Some(status) = status else {
        if control.is_canceled() {
            remove_dir_retrying(&job.temp_dir).await;
            return Ok((Outcome::Canceled, None));
        }
        return Ok((Outcome::Paused { bytes: bytes_seen }, None));
    };
    let status = status?;
    if !status.success() {
        let line = stderr_text
            .lines()
            .rev()
            .find(|l| l.contains("ERROR"))
            .or_else(|| stderr_text.lines().rev().find(|l| !l.trim().is_empty()))
            .unwrap_or("error desconocido");
        return Err(Error::Video(
            line.trim().trim_start_matches("ERROR:").trim().to_owned(),
        ));
    }

    let path = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .final_path
        .clone()
        .map(PathBuf::from)
        .filter(|p| p.starts_with(&job.out_dir) && p.is_file())
        .ok_or_else(|| Error::Video("yt-dlp terminó sin producir un archivo".into()))?;
    let bytes = tokio::fs::metadata(&path).await?.len();
    remove_dir_retrying(&job.temp_dir).await;
    if let Some(p) = &progress {
        p.store(bytes, Ordering::Relaxed);
    }
    Ok((
        Outcome::Completed(Report {
            bytes,
            downloaded: bytes_seen.min(bytes),
            resumed: 0,
            elapsed: started.elapsed(),
            mode: Mode::Single,
        }),
        Some(path),
    ))
}

/// Mata a yt-dlp y a todo lo que lanzó. El `yt-dlp.exe` oficial es un arrancador que ejecuta el
/// programa real como proceso hijo: matar solo al arrancador deja al hijo escribiendo en la carpeta
/// de intermedios, y al reanudar habría dos procesos sobre los mismos archivos.
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

/// Borra una carpeta aunque algún archivo siga bloqueado un instante tras matar el proceso.
async fn remove_dir_retrying(dir: &Path) {
    for _ in 0..30 {
        match tokio::fs::remove_dir_all(dir).await {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Carpeta de intermedios de una descarga: estable para poder retomarla, sin caracteres raros.
pub fn temp_dir_for(out_dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect();
    out_dir.join(format!(".w11dl-{safe}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> YtDlpJob {
        YtDlpJob {
            python: "python".into(),
            ffmpeg: Some("ffmpeg".into()),
            page: "https://sitio.test/v?x=1".into(),
            out_dir: "/descargas".into(),
            stem: "100% Vídeo".into(),
            temp_dir: "/descargas/.w11dl-abc".into(),
            rate_limit: 0,
            cookies: None,
        }
    }

    fn joined(job: &YtDlpJob) -> String {
        build_args(job)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn argumentos_seguros_y_completos() {
        let a = joined(&job());
        assert!(a.contains("--no-playlist") && a.contains("--no-cache-dir"));
        assert!(a.contains("-N 8"), "fragmentos en paralelo: {a}");
        assert!(a.contains("-f bv+ba/b"), "{a}");
        assert!(a.contains("-P home:/descargas") && a.contains("-P temp:/descargas/.w11dl-abc"));
        assert!(
            a.contains("-o 100%% Vídeo.%(ext)s"),
            "el % del título se escapa: {a}"
        );
        assert!(
            a.ends_with("-- https://sitio.test/v?x=1"),
            "la URL va tras `--`: {a}"
        );
        assert!(!a.contains("--limit-rate") && !a.contains("--cookies"));
        assert!(a.contains("--ffmpeg-location ffmpeg"));
    }

    #[test]
    fn yt_dlp_autonomo_se_lanza_sin_el_modulo_de_python() {
        let mut j = job();
        j.python = "C:/app/tools/yt-dlp.exe".into();
        assert!(!joined(&j).starts_with("-m yt_dlp"), "{}", joined(&j));
        assert!(joined(&j).starts_with("--no-playlist"));
        assert!(joined(&job()).starts_with("-m yt_dlp --no-playlist"));
    }

    #[test]
    fn sin_ffmpeg_pide_un_formato_completo_y_con_limite_lo_pasa() {
        let mut j = job();
        j.ffmpeg = None;
        j.rate_limit = 1_048_576;
        j.cookies = Some("/c.txt".into());
        let a = joined(&j);
        assert!(a.contains("-f b ") && !a.contains("bv+ba"), "{a}");
        assert!(a.contains("--limit-rate 1048576"));
        assert!(a.contains("--cookies /c.txt"));
        assert!(!a.contains("--ffmpeg-location"));
    }

    #[test]
    fn progreso_y_valores_no_numericos() {
        assert_eq!(
            parse_progress("W11DL 137 12345"),
            Some(("137".into(), 12345))
        );
        assert_eq!(
            parse_progress("W11DL hls-720 2048.0"),
            Some(("hls-720".into(), 2048))
        );
        for bad in [
            "W11DL 137 NA",
            "W11DL 137",
            "otra cosa 1 2",
            "",
            "/ruta/archivo.mp4",
        ] {
            assert_eq!(parse_progress(bad), None, "{bad}");
        }
    }

    #[test]
    fn carpeta_temporal_estable_y_sin_caracteres_raros() {
        let d = temp_dir_for(Path::new("/d"), "ab-12/../x y");
        assert_eq!(
            d,
            Path::new("/d/.w11dl-ab-12x-y".replace("x-y", "xy").as_str())
        );
        assert_eq!(
            temp_dir_for(Path::new("/d"), "id"),
            temp_dir_for(Path::new("/d"), "id")
        );
    }
}
