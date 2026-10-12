//! Entorno de yt-dlp que gestiona la aplicación. Se crea con `w11dl-host setup`:
//! un entorno virtual propio con yt-dlp fijado por hash. Así no depende del PATH ni del Python del usuario.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Lock con la versión y los hashes SHA-256 de PyPI.
pub const REQUIREMENTS_LOCK: &str = include_str!("../packaging/requirements.lock");
/// Versión de yt-dlp que fija el lock (formato de PyPI).
pub const YTDLP_PIN: &str = "2026.8.19";
const MIN_PYTHON: (u32, u32) = (3, 10);

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

/// Primer Python 3.10+ que responda. Devuelve el programa y los argumentos previos.
fn find_base_python() -> Result<(String, Vec<String>)> {
    for (program, args) in base_candidates() {
        let probe = Command::new(&program)
            .args(&args)
            .args(["-c", "import sys; print('%d.%d' % sys.version_info[:2])"])
            .output();
        if let Ok(out) = probe {
            if out.status.success()
                && parse_python_version(&String::from_utf8_lossy(&out.stdout)).is_some()
            {
                return Ok((program, args));
            }
        }
    }
    Err(Error::Setup(
        "no encuentro Python 3.10 o superior; instálalo y vuelve a ejecutar setup".into(),
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
    let python = venv_python(data);
    if !python.exists() {
        return Status {
            ready: false,
            python: None,
            message: "entorno no creado: ejecuta w11dl-host setup".into(),
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
    fn lock_fija_version_y_hashes() {
        assert!(REQUIREMENTS_LOCK.contains("yt-dlp==2026.8.19"));
        assert_eq!(REQUIREMENTS_LOCK.matches("--hash=sha256:").count(), 2);
        assert!(
            REQUIREMENTS_LOCK
                .contains("1d57897e94c6665a0a6f9bc54b34e584284e32c034ffab3a7df25d8f7b24eedf")
        );
    }

    #[test]
    fn sin_entorno_el_estado_no_esta_listo() {
        let dir = tempfile::tempdir().unwrap();
        let s = check(dir.path());
        assert!(!s.ready);
        assert!(s.message.contains("setup"));
    }
}
