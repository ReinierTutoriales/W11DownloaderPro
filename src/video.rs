//! Resolución de vídeo con yt-dlp. Obtiene los formatos de una página o archivo sin descargar nada,
//! descarta DRM y streams segmentados (fase 2) y elige el mejor archivo directo para el motor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::naming;

/// Versión de yt-dlp con la que se han probado estos cambios (la vigente en PyPI).
pub const YTDLP_VERSION: &str = "2026.08.19";
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_JSON: usize = 16 * 1024 * 1024;
/// Contenedores de vídeo que el motor descarga directamente.
const DIRECT_VIDEO_EXTS: &[&str] = &["mp4", "webm", "mkv", "mov"];

#[derive(Debug, Deserialize)]
struct Info {
    title: Option<String>,
    #[serde(flatten)]
    single: Format,
    formats: Option<Vec<Format>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Format {
    pub url: Option<String>,
    pub ext: Option<String>,
    pub protocol: Option<String>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub height: Option<u32>,
    pub has_drm: Option<bool>,
    pub http_headers: Option<HashMap<String, String>>,
}

/// Formato elegido, listo para entregarse al motor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub url: String,
    pub filename: String,
    pub height: Option<u32>,
    pub headers: Vec<(String, String)>,
}

/// Opciones de la consulta a yt-dlp.
#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    /// Intérprete de Python que tiene yt-dlp instalado (`python -m yt_dlp`).
    pub python: Option<PathBuf>,
    /// Cookies en formato Netscape. Solo se usan si se indica explícitamente.
    pub cookies: Option<PathBuf>,
}

/// Consulta yt-dlp y devuelve el mejor formato directo sin DRM.
pub async fn probe(page: &str, opts: &ProbeOptions) -> Result<Choice> {
    // Sin intérprete configurado no se adivina: un `python` del PATH sin yt-dlp daría errores confusos.
    let python = opts.python.clone().ok_or_else(|| {
        Error::Video(format!(
            "vídeo no disponible: falta el entorno de yt-dlp. Ejecuta w11dl-host setup o indica --python (Python 3.10+: {})",
            crate::runtime::PYTHON_DOWNLOAD_URL
        ))
    })?;
    let mut cmd = Command::new(&python);
    cmd.arg("-m").arg("yt_dlp").args([
        "--dump-single-json",
        "--skip-download",
        "--no-playlist",
        "--no-warnings",
        "--no-cache-dir",
    ]);
    if let Some(cookies) = &opts.cookies {
        cmd.arg("--cookies").arg(cookies);
    }
    cmd.arg("--").arg(page);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| Error::Video("yt-dlp tardó demasiado".into()))?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                Error::Video(format!(
                    "vídeo no disponible: el Python configurado no existe. Ejecuta w11dl-host setup ({})",
                    crate::runtime::PYTHON_DOWNLOAD_URL
                ))
            }
            _ => Error::Video(format!("no se pudo ejecutar yt-dlp: {e}")),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("error desconocido");
        return Err(Error::Video(first.trim().to_owned()));
    }
    if output.stdout.len() > MAX_JSON {
        return Err(Error::Video("respuesta de yt-dlp demasiado grande".into()));
    }

    let info: Info = serde_json::from_slice(&output.stdout)?;
    select(info)
}

/// Elige el formato directo. Separado de `probe` para poder probarlo sin ejecutar yt-dlp.
fn select(info: Info) -> Result<Choice> {
    let mut candidates: Vec<Format> = info.formats.unwrap_or_default();
    // Un archivo directo puede venir sin lista de formatos: el propio objeto es el formato.
    if candidates.is_empty() && info.single.url.is_some() {
        candidates.push(info.single);
    }

    let best = candidates
        .iter()
        .filter(|f| is_direct_video(f))
        .max_by_key(|f| (has_audio(f), f.height.unwrap_or(0)))
        .ok_or_else(|| Error::Video("no hay un formato de vídeo directo sin DRM".into()))?;

    let url = best.url.clone().unwrap_or_default();
    let ext = best.ext.clone().unwrap_or_else(|| "mp4".into());
    let stem = info
        .title
        .as_deref()
        .and_then(naming::sanitize)
        .unwrap_or_else(|| "video".into());
    let headers = best
        .http_headers
        .iter()
        .flatten()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();

    Ok(Choice {
        url,
        filename: format!("{stem}.{ext}"),
        height: best.height,
        headers,
    })
}

fn is_direct_video(f: &Format) -> bool {
    let url_ok = f
        .url
        .as_deref()
        .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
    let protocol_ok = matches!(f.protocol.as_deref(), None | Some("http") | Some("https"));
    let ext_ok = f
        .ext
        .as_deref()
        .is_some_and(|e| DIRECT_VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()));
    let has_video = f.vcodec.as_deref() != Some("none");
    url_ok && protocol_ok && ext_ok && has_video && f.has_drm != Some(true)
}

fn has_audio(f: &Format) -> bool {
    f.acodec.as_deref() != Some("none")
}

/// Cabeceras que el motor gestiona por su cuenta y no deben copiarse de yt-dlp.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "range" | "if-range" | "connection" | "content-length" | "accept-encoding"
    )
}

/// Python con yt-dlp por defecto: `W11DL_PYTHON` si existe (para desarrollo), o el entorno que
/// creó `w11dl-host setup`.
pub fn default_python() -> Option<PathBuf> {
    std::env::var_os("W11DL_PYTHON")
        .map(PathBuf::from)
        .or_else(crate::runtime::configured_python)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(json: &str) -> Info {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn elige_el_mayor_formato_con_audio_y_vídeo() {
        let c = select(info(r#"{
            "title": "Clip",
            "formats": [
                {"url":"https://x.test/a.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":1080},
                {"url":"https://x.test/b.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"mp4a","height":720},
                {"url":"https://x.test/c.webm","ext":"webm","protocol":"https","vcodec":"vp9","acodec":"opus","height":480}
            ]}"#))
        .unwrap();
        assert_eq!(
            c.url, "https://x.test/b.mp4",
            "con audio gana aunque sea menor"
        );
        assert_eq!(c.filename, "Clip.mp4");
        assert_eq!(c.height, Some(720));
    }

    #[test]
    fn descarta_drm_y_streams_segmentados() {
        let r = select(info(
            r#"{
            "title": "x",
            "formats": [
                {"url":"https://x.test/d.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"mp4a","height":1080,"has_drm":true},
                {"url":"https://x.test/p.m3u8","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1","acodec":"mp4a","height":1080}
            ]}"#,
        ));
        assert!(matches!(r, Err(Error::Video(_))), "{r:?}");
    }

    #[test]
    fn solo_audio_no_es_vídeo() {
        let r = select(info(
            r#"{"title":"x","formats":[{"url":"https://x.test/a.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a"}]}"#,
        ));
        assert!(r.is_err());
    }

    #[test]
    fn archivo_directo_sin_lista_de_formatos() {
        let c = select(info(r#"{"title":"Peli","url":"https://x.test/peli.mkv","ext":"mkv","vcodec":"unknown","acodec":"unknown","http_headers":{"Referer":"https://x.test/","Range":"bytes=0-"}}"#)).unwrap();
        assert_eq!(c.filename, "Peli.mkv");
        assert_eq!(
            c.headers,
            vec![("Referer".to_owned(), "https://x.test/".to_owned())],
            "Range se descarta"
        );
    }

    #[test]
    fn titulo_con_caracteres_prohibidos_se_limpia() {
        let c = select(info(
            r#"{"title":"a/b:c?","url":"https://x.test/v.mp4","ext":"mp4","vcodec":"avc1"}"#,
        ))
        .unwrap();
        assert_eq!(
            c.filename, "b_c_.mp4",
            "solo el último componente, sin caracteres prohibidos"
        );
    }
}
