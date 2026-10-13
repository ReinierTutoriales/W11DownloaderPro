//! Herramientas de vídeo de la aplicación. En la versión empaquetada vienen incluidas junto al
//! ejecutable (carpeta `tools`: yt-dlp, ffmpeg y ffprobe) y no hay que instalar nada. Si no están,
//! `w11dl-host setup` crea un entorno virtual propio con yt-dlp fijado por hash, sin depender del
//! PATH ni del Python del usuario.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Página oficial de descarga de Python: el enlace que se muestra cuando falta la dependencia.
pub const PYTHON_DOWNLOAD_URL: &str = "https://www.python.org/downloads/";

/// Lock con la versión y los hashes SHA-256 de PyPI.
pub const REQUIREMENTS_LOCK: &str = include_str!("../packaging/requirements.lock");
/// Versión de yt-dlp que fija el lock (formato de PyPI).
pub const YTDLP_PIN: &str = "2026.8.19";
const MIN_PYTHON: (u32, u32) = (3, 10);

/// Carpeta `tools` que acompaña a un ejecutable (versión empaquetada), si existe.
pub fn bundled_dir_near(exe_dir: &Path) -> Option<PathBuf> {
    let dir = exe_dir.join("tools");
    dir.is_dir().then_some(dir)
}

fn bundled_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    bundled_dir_near(exe.parent()?)
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// yt-dlp incluido en la versión empaquetada.
pub fn bundled_ytdlp() -> Option<PathBuf> {
    let path = bundled_dir()?.join(exe_name("yt-dlp"));
    path.is_file().then_some(path)
}

/// ffmpeg incluido en la versión empaquetada (junto a su ffprobe, que yt-dlp busca al lado).
pub fn bundled_ffmpeg() -> Option<PathBuf> {
    let path = bundled_dir()?.join(exe_name("ffmpeg"));
    path.is_file().then_some(path)
}

/// ¿Es `program` un yt-dlp autónomo (ejecutable propio) y no un Python con el módulo `yt_dlp`?
pub fn is_standalone_ytdlp(program: &Path) -> bool {
    program.file_stem().is_some_and(|stem| {
        stem.to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("yt-dlp")
    })
}

/// Argumentos previos para lanzar yt-dlp: ninguno si es autónomo, `-m yt_dlp` si es un Python.
pub fn ytdlp_prefix(program: &Path) -> &'static [&'static str] {
    if is_standalone_ytdlp(program) {
        &[]
    } else {
        &["-m", "yt_dlp"]
    }
}

/// Estado del entorno de vídeo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub ready: bool,
    pub python: Option<PathBuf>,
    pub message: String,
}

/// Carpeta de datos de la aplicación: `%LOCALAPPDATA%\W11DownloaderPro` en Windows.
pub fn data_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(base).join("W11DownloaderPro");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/W11DownloaderPro");
    }
    PathBuf::from("W11DownloaderPro-data")
}

fn venv_dir(data: &Path) -> PathBuf {
    data.join("venv")
}

fn venv_python(data: &Path) -> PathBuf {
    if cfg!(windows) {
        venv_dir(data).join("Scripts").join("python.exe")
    } else {
        venv_dir(data).join("bin").join("python")
    }
}

fn config_path(data: &Path) -> PathBuf {
    data.join("config.json")
}

/// Intérprete de yt-dlp preparado por `setup`, si existe y sigue funcionando.
pub fn configured_python() -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_path(&data_dir())).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let python = PathBuf::from(value.get("python")?.as_str()?);
    python.exists().then_some(python)
}

/// Lee `major.minor` de `python -c ...` y comprueba que sea 3.10 o superior.
pub fn parse_python_version(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    (parts.next().is_none() && (major, minor) >= MIN_PYTHON).then_some((major, minor))
}

/// Compara versiones de yt-dlp ignorando los ceros a la izquierda (`2026.08.19` == `2026.8.19`).
pub fn same_version(a: &str, b: &str) -> bool {
    let norm =
        |v: &str| -> Option<Vec<u64>> { v.trim().split('.').map(|p| p.parse().ok()).collect() };
    matches!((norm(a), norm(b)), (Some(x), Some(y)) if x == y)
}

/// Candidatos de intérprete base, en orden. `W11DL_BASE_PYTHON` tiene prioridad.
fn base_candidates() -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("W11DL_BASE_PYTHON") {
        out.push((p.to_string_lossy().into_owned(), Vec::new()));
    }
    if cfg!(windows) {
        out.push(("py".into(), vec!["-3".into()]));
    }
    out.push(("python3".into(), Vec::new()));
    out.push(("python".into(), Vec::new()));
    out
}

/// Primer Python 3.10+ que responda. Si hay Python pero es antiguo, lo dice explícitamente.
fn find_base_python() -> Result<(String, Vec<String>)> {
    let mut found_old: Option<String> = None;
    for (program, args) in base_candidates() {
        let probe = Command::new(&program)
            .args(&args)
            .args(["-c", "import sys; print('%d.%d' % sys.version_info[:2])"])
            .output();
        let Ok(out) = probe else { continue };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if parse_python_version(&text).is_some() {
            return Ok((program, args));
        }
        found_old.get_or_insert(format!("{program} {text}"));
    }
    Err(missing_python_error(found_old.as_deref()))
}

/// Motivo claro y enlace cuando no hay un Python utilizable.
pub fn missing_python_error(found_old: Option<&str>) -> Error {
    let reason = match found_old {
        Some(found) => format!("el Python que hay ({found}) es anterior a 3.10"),
        None => "no encuentro Python en este equipo".to_owned(),
    };
    Error::Setup(format!(
        "{reason}. Instala Python 3.10 o superior desde {PYTHON_DOWNLOAD_URL} y ejecuta w11dl-host setup"
    ))
}

fn run(program: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Setup(format!("no se pudo ejecutar {}: {e}", program.display())))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("error desconocido");
        return Err(Error::Setup(first.trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Crea el entorno, instala yt-dlp con verificación de hash y comprueba la versión.
pub fn setup(data: &Path) -> Result<Status> {
    // La versión empaquetada ya trae yt-dlp: no hay entorno que crear.
    if let Some(status) = check_bundled() {
        return Ok(status);
    }
    std::fs::create_dir_all(data)?;
    let python = venv_python(data);
    if !python.exists() {
        let (program, args) = find_base_python()?;
        let venv = venv_dir(data);
        let mut cmd = Command::new(&program);
        cmd.args(&args).args(["-m", "venv"]).arg(&venv);
        let out = cmd.output().map_err(|e| Error::Setup(e.to_string()))?;
        if !out.status.success() {
            return Err(Error::Setup("no se pudo crear el entorno virtual".into()));
        }
    }

    let lock = data.join("requirements.lock");
    std::fs::write(&lock, REQUIREMENTS_LOCK)?;
    run(
        &python,
        &[
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-input",
            // Sin caché (no se reutiliza un archivo sin verificar), solo wheels (sin código de construcción)
            // y con hashes obligatorios.
            "--no-cache-dir",
            "--only-binary=:all:",
            "--require-hashes",
            "-r",
            &lock.display().to_string(),
        ],
    )?;

    let status = check(data);
    if status.ready {
        std::fs::write(
            config_path(data),
            serde_json::json!({ "python": python.display().to_string(), "yt_dlp": YTDLP_PIN })
                .to_string(),
        )?;
    }
    Ok(status)
}

/// Estado actual sin cambiar nada: el entorno existe y yt-dlp responde con la versión fijada.
pub fn check(data: &Path) -> Status {
    if let Some(status) = check_bundled() {
        return status;
    }
    let python = venv_python(data);
    if !python.exists() {
        let message = match find_base_python() {
            Ok(_) => "entorno no creado: ejecuta w11dl-host setup".to_owned(),
            Err(e) => format!("vídeo no disponible: {e}"),
        };
        return Status {
            ready: false,
            python: None,
            message,
        };
    }
    match run(&python, &["-m", "yt_dlp", "--version"]) {
        Ok(version) if same_version(&version, YTDLP_PIN) => Status {
            ready: true,
            python: Some(python),
            message: format!("yt-dlp {} listo", version.trim()),
        },
        Ok(version) => Status {
            ready: false,
            python: Some(python),
            message: format!(
                "yt-dlp {} no es la versión fijada {YTDLP_PIN}: ejecuta setup",
                version.trim()
            ),
        },
        Err(e) => Status {
            ready: false,
            python: Some(python),
            message: format!("yt-dlp no responde: {e}"),
        },
    }
}

/// Estado del yt-dlp incluido, o `None` si esta no es una versión empaquetada.
fn check_bundled() -> Option<Status> {
    let program = bundled_ytdlp()?;
    Some(match run(&program, &["--version"]) {
        Ok(version) if same_version(&version, YTDLP_PIN) => Status {
            ready: true,
            python: Some(program),
            message: format!("yt-dlp {} incluido y listo", version.trim()),
        },
        Ok(version) => Status {
            ready: false,
            python: Some(program),
            message: format!(
                "el yt-dlp incluido es {} y se esperaba {YTDLP_PIN}",
                version.trim()
            ),
        },
        Err(e) => Status {
            ready: false,
            python: Some(program),
            message: format!("el yt-dlp incluido no responde: {e}"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_minimo_3_10() {
        assert_eq!(parse_python_version("3.13\n"), Some((3, 13)));
        assert_eq!(parse_python_version("3.10"), Some((3, 10)));
        assert_eq!(parse_python_version("3.9"), None);
        assert_eq!(parse_python_version("2.7"), None);
        assert_eq!(parse_python_version("3.13.1"), None, "solo major.minor");
        assert_eq!(parse_python_version("basura"), None);
    }

    #[test]
    fn versiones_de_ytdlp_se_comparan_sin_ceros() {
        assert!(same_version("2026.08.19\n", "2026.8.19"));
        assert!(!same_version("2026.08.20", "2026.8.19"));
        assert!(!same_version("", "2026.8.19"));
    }

    #[test]
    fn yt_dlp_autonomo_o_modulo_de_python() {
        for standalone in [
            "yt-dlp.exe",
            "/x/tools/yt-dlp",
            "C:/t/YT-DLP.EXE",
            "yt-dlp_linux",
        ] {
            assert!(is_standalone_ytdlp(Path::new(standalone)), "{standalone}");
            assert!(ytdlp_prefix(Path::new(standalone)).is_empty());
        }
        for python in ["python", "python3.13", "/venv/bin/python", "py.exe"] {
            assert!(!is_standalone_ytdlp(Path::new(python)), "{python}");
            assert_eq!(ytdlp_prefix(Path::new(python)), ["-m", "yt_dlp"]);
        }
    }

    #[test]
    fn herramientas_incluidas_solo_si_hay_carpeta_tools() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(bundled_dir_near(dir.path()), None);
        std::fs::create_dir(dir.path().join("tools")).unwrap();
        assert_eq!(bundled_dir_near(dir.path()), Some(dir.path().join("tools")));
    }

    #[test]
    fn lock_fija_version_y_hashes() {
        assert!(REQUIREMENTS_LOCK.contains("yt-dlp==2026.8.19"));
        assert_eq!(REQUIREMENTS_LOCK.matches("--hash=sha256:").count(), 2);
        assert!(
            REQUIREMENTS_LOCK
                .contains("1d57897e94c6665a0a6f9bc54b34e584284e32c034ffab3a7df25d8f7b24eedf")
        );
    }

    #[test]
    fn falta_python_da_motivo_y_enlace() {
        let none = missing_python_error(None).to_string();
        assert!(none.contains("no encuentro Python"), "{none}");
        assert!(none.contains(PYTHON_DOWNLOAD_URL), "{none}");
        let old = missing_python_error(Some("python3 3.9")).to_string();
        assert!(
            old.contains("python3 3.9") && old.contains("anterior a 3.10"),
            "{old}"
        );
        assert!(old.contains(PYTHON_DOWNLOAD_URL));
    }

    #[test]
    fn sin_entorno_el_estado_no_esta_listo() {
        let dir = tempfile::tempdir().unwrap();
        let s = check(dir.path());
        assert!(!s.ready);
        assert!(s.message.contains("setup"));
    }
}
