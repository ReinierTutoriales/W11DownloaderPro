//! Resolución de vídeo con yt-dlp. Obtiene los formatos de una página o archivo sin descargar nada,
//! descarta DRM y streams segmentados (fase 2) y elige el mejor archivo directo para el motor.
//! Si el sitio entrega imagen y sonido por separado (p. ej. YouTube) y hay ffmpeg, elige el mejor de
//! cada uno para unirlos sin recodificar.

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
    /// Tasa de audio en kbit/s, para elegir el mejor sonido.
    pub abr: Option<f64>,
    pub has_drm: Option<bool>,
    pub http_headers: Option<HashMap<String, String>>,
}

/// Pista de sonido que se descarga aparte y se une con la imagen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPart {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Formato elegido, listo para entregarse al motor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub url: String,
    pub filename: String,
    pub height: Option<u32>,
    pub headers: Vec<(String, String)>,
    /// Si existe, `url` es solo imagen y el sonido va aparte: hay que unirlos con ffmpeg.
    pub audio: Option<AudioPart>,
}

/// Opciones de la consulta a yt-dlp.
#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    /// Intérprete de Python que tiene yt-dlp instalado (`python -m yt_dlp`).
    pub python: Option<PathBuf>,
    /// Cookies en formato Netscape. Solo se usan si se indica explícitamente.
    pub cookies: Option<PathBuf>,
    /// ffmpeg disponible para unir imagen y sonido. Sin él solo se eligen formatos completos.
    pub ffmpeg: Option<PathBuf>,
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
    select(info, opts.ffmpeg.is_some())
}

/// Elige el formato directo. Separado de `probe` para poder probarlo sin ejecutar yt-dlp.
/// `can_merge` indica si hay ffmpeg para unir imagen y sonido de formatos separados.
fn select(info: Info, can_merge: bool) -> Result<Choice> {
    let mut candidates: Vec<Format> = info.formats.unwrap_or_default();
    // Un archivo directo puede venir sin lista de formatos: el propio objeto es el formato.
    if candidates.is_empty() && info.single.url.is_some() {
        candidates.push(info.single);
    }

    let direct: Vec<&Format> = candidates.iter().filter(|f| is_direct_video(f)).collect();
    // Un archivo sin sonido no es lo que el usuario espera: nunca se elige en silencio.
    let combined = direct
        .iter()
        .filter(|f| has_audio(f))
        .max_by_key(|f| f.height.unwrap_or(0))
        .copied();
    let pair = best_pair(&direct, &candidates);

    let stem = info
        .title
        .as_deref()
        .and_then(naming::sanitize)
        .unwrap_or_else(|| "video".into());

    // La pareja separada gana solo si da más resolución que el mejor formato completo.
    let merged = match (combined, &pair) {
        (Some(c), Some((v, _, _))) => can_merge && v.height.unwrap_or(0) > c.height.unwrap_or(0),
        (None, Some(_)) => true,
        _ => false,
    };
    if merged && !can_merge {
        return Err(Error::Video(format!(
            "este vídeo entrega imagen y sonido por separado y necesita ffmpeg para unirlos. \
             Instálalo desde {FFMPEG_DOWNLOAD_URL} (o indica su ruta en W11DL_FFMPEG) y vuelve a intentarlo"
        )));
    }
    if merged {
        let (video, audio, ext) = pair.expect("merged implica pareja");
        return Ok(Choice {
            url: video.url.clone().unwrap_or_default(),
            filename: format!("{stem}.{ext}"),
            height: video.height,
            headers: clean_headers(video),
            audio: Some(AudioPart {
                url: audio.url.clone().unwrap_or_default(),
                headers: clean_headers(audio),
            }),
        });
    }
    let best = combined
        .ok_or_else(|| Error::Video("no hay un formato de vídeo directo sin DRM".into()))?;
    Ok(Choice {
        url: best.url.clone().unwrap_or_default(),
        filename: format!(
            "{stem}.{}",
            best.ext.clone().unwrap_or_else(|| "mp4".into())
        ),
        height: best.height,
        headers: clean_headers(best),
        audio: None,
    })
}

fn clean_headers(f: &Format) -> Vec<(String, String)> {
    f.http_headers
        .iter()
        .flatten()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

/// Mejor imagen sin sonido con el mejor sonido compatible, y el contenedor resultante.
fn best_pair<'a>(
    direct: &[&'a Format],
    all: &'a [Format],
) -> Option<(&'a Format, &'a Format, &'static str)> {
    // A igual resolución se prefiere MP4: lo reproduce todo.
    let video = direct
        .iter()
        .filter(|f| !has_audio(f))
        .max_by_key(|f| (f.height.unwrap_or(0), f.ext.as_deref() == Some("mp4")))
        .copied()?;
    let audios: Vec<&Format> = all.iter().filter(|f| is_direct_audio(f)).collect();
    let by_rate = |a: &&&Format, b: &&&Format| {
        a.abr
            .unwrap_or(0.0)
            .partial_cmp(&b.abr.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    let natural = |a: &Format| match video.ext.as_deref() {
        Some("mp4") => a.acodec.as_deref().is_some_and(|c| c.starts_with("mp4a")),
        Some("webm") => a.ext.as_deref() == Some("webm"),
        _ => false,
    };
    let compatible = audios
        .iter()
        .filter(|a| natural(a))
        .max_by(by_rate)
        .copied();
    match compatible {
        Some(audio) => {
            let container = if video.ext.as_deref() == Some("webm") {
                "webm"
            } else {
                "mp4"
            };
            Some((video, audio, container))
        }
        None => audios
            .iter()
            .max_by(by_rate)
            .copied()
            .map(|audio| (video, audio, "mkv")),
    }
}

fn is_direct_audio(f: &Format) -> bool {
    let url_ok = f
        .url
        .as_deref()
        .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"));
    let protocol_ok = matches!(f.protocol.as_deref(), None | Some("http") | Some("https"));
    f.vcodec.as_deref() == Some("none")
        && has_audio(f)
        && url_ok
        && protocol_ok
        && f.has_drm != Some(true)
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

/// Página oficial de ffmpeg con las descargas para Windows.
pub const FFMPEG_DOWNLOAD_URL: &str = "https://ffmpeg.org/download.html";

/// Busca ffmpeg: `W11DL_FFMPEG`, la carpeta de datos de la aplicación y el PATH, en ese orden.
/// Solo devuelve una ruta que responde a `-version`.
pub async fn find_ffmpeg() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("W11DL_FFMPEG") {
        candidates.push(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    candidates.push(crate::runtime::data_dir().join("ffmpeg").join(name));
    candidates.push(PathBuf::from(name));
    for candidate in candidates {
        let ok = Command::new(&candidate)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .is_ok_and(|status| status.success());
        if ok {
            return Some(candidate);
        }
    }
    None
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

    /// Sin ffmpeg: solo formatos completos.
    fn select_solo(info: Info) -> Result<Choice> {
        select(info, false)
    }

    const YOUTUBE_LIKE: &str = r#"{"title":"Zoo","formats":[
        {"url":"https://x.test/18.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"mp4a","height":360},
        {"url":"https://x.test/v1080.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":1080},
        {"url":"https://x.test/v1080.webm","ext":"webm","protocol":"https","vcodec":"vp9","acodec":"none","height":1080},
        {"url":"https://x.test/a-low.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.5","abr":49},
        {"url":"https://x.test/a-hi.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.2","abr":130},
        {"url":"https://x.test/a-opus.webm","ext":"webm","protocol":"https","vcodec":"none","acodec":"opus","abr":160},
        {"url":"https://x.test/drm.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":2160,"has_drm":true}
    ]}"#;

    #[test]
    fn con_ffmpeg_une_la_mejor_imagen_con_el_mejor_sonido_compatible() {
        let c = select(info(YOUTUBE_LIKE), true).unwrap();
        assert_eq!(
            c.url, "https://x.test/v1080.mp4",
            "mp4 gana a webm a igual resolución; el DRM se ignora"
        );
        let audio = c.audio.expect("debe llevar sonido aparte");
        assert_eq!(
            audio.url, "https://x.test/a-hi.m4a",
            "el m4a de más tasa, compatible con mp4"
        );
        assert_eq!(c.filename, "Zoo.mp4");
        assert_eq!(c.height, Some(1080));
    }

    #[test]
    fn sin_ffmpeg_prefiere_el_completo_y_sin_completo_explica_la_dependencia() {
        let c = select(info(YOUTUBE_LIKE), false).unwrap();
        assert_eq!(c.url, "https://x.test/18.mp4");
        assert!(c.audio.is_none());

        let solo_separados = r#"{"title":"x","formats":[
            {"url":"https://x.test/v.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":720},
            {"url":"https://x.test/a.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a","abr":128}]}"#;
        let Err(Error::Video(m)) = select(info(solo_separados), false) else {
            panic!("debía pedir ffmpeg");
        };
        assert!(
            m.contains("ffmpeg") && m.contains(FFMPEG_DOWNLOAD_URL),
            "{m}"
        );
    }

    #[test]
    fn el_completo_gana_si_la_pareja_no_da_mas_resolucion() {
        let json = r#"{"title":"x","formats":[
            {"url":"https://x.test/c.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"mp4a","height":720},
            {"url":"https://x.test/v.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":720},
            {"url":"https://x.test/a.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a","abr":128}]}"#;
        let c = select(info(json), true).unwrap();
        assert_eq!(c.url, "https://x.test/c.mp4");
        assert!(c.audio.is_none());
    }

    #[test]
    fn sin_sonido_compatible_usa_mkv() {
        let json = r#"{"title":"x","formats":[
            {"url":"https://x.test/v.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":720},
            {"url":"https://x.test/a.webm","ext":"webm","protocol":"https","vcodec":"none","acodec":"opus","abr":128}]}"#;
        let c = select(info(json), true).unwrap();
        assert_eq!(c.filename, "x.mkv", "opus no cabe en mp4 sin recodificar");
        assert!(c.audio.is_some());
    }

    #[test]
    fn elige_el_mayor_formato_con_audio_y_vídeo() {
        let c = select_solo(info(r#"{
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
        let r = select_solo(info(
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
    fn nunca_elige_en_silencio_un_vídeo_sin_sonido() {
        // Como YouTube: imagen y sonido en formatos separados.
        let r = select_solo(info(
            r#"{"title":"x","formats":[
                {"url":"https://x.test/v.mp4","ext":"mp4","protocol":"https","vcodec":"avc1","acodec":"none","height":1080},
                {"url":"https://x.test/a.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a"}
            ]}"#,
        ));
        let Err(Error::Video(message)) = r else {
            panic!("debía fallar, no devolver un vídeo mudo: {r:?}");
        };
        assert!(
            message.contains("por separado") && message.contains("ffmpeg"),
            "{message}"
        );
    }

    #[test]
    fn solo_audio_no_es_vídeo() {
        let r = select_solo(info(
            r#"{"title":"x","formats":[{"url":"https://x.test/a.m4a","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a"}]}"#,
        ));
        assert!(r.is_err());
    }

    #[test]
    fn archivo_directo_sin_lista_de_formatos() {
        let c = select_solo(info(r#"{"title":"Peli","url":"https://x.test/peli.mkv","ext":"mkv","vcodec":"unknown","acodec":"unknown","http_headers":{"Referer":"https://x.test/","Range":"bytes=0-"}}"#)).unwrap();
        assert_eq!(c.filename, "Peli.mkv");
        assert_eq!(
            c.headers,
            vec![("Referer".to_owned(), "https://x.test/".to_owned())],
            "Range se descarta"
        );
    }

    #[test]
    fn titulo_con_caracteres_prohibidos_se_limpia() {
        let c = select_solo(info(
            r#"{"title":"a/b:c?","url":"https://x.test/v.mp4","ext":"mp4","vcodec":"avc1"}"#,
        ))
        .unwrap();
        assert_eq!(
            c.filename, "b_c_.mp4",
            "solo el último componente, sin caracteres prohibidos"
        );
    }
}
