//! Host de mensajería nativa: lo lanza el navegador y recibe las descargas capturadas.
//!
//! Uso:
//! - Sin argumentos: modo servicio (lo ejecuta el navegador).
//! - `register [--extension-id <id>] [--firefox-id <id>]`: escribe los manifiestos y los registra.
//! - `unregister`: quita el registro.

use std::path::PathBuf;
use std::process::ExitCode;

use w11dl::native;
use w11dl::register;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("register") => finish(run_register(&args)),
        Some("unregister") => {
            finish(register::unregister(&manifest_dir()).map(|()| "Registro eliminado.".into()))
        }
        _ => {
            let out_dir = out_dir_from_args(&args).unwrap_or_else(default_out_dir);
            match native::serve(tokio::io::stdin(), tokio::io::stdout(), out_dir).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("w11dl-host: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn run_register(args: &[String]) -> std::io::Result<String> {
    let targets = register::Targets {
        chromium_id: value_of(args, "--extension-id"),
        firefox_id: value_of(args, "--firefox-id"),
    };
    let host_exe = std::env::current_exe()?;
    let written = register::register(&host_exe, &targets, &manifest_dir())?;
    let list: Vec<String> = written.iter().map(|p| p.display().to_string()).collect();
    Ok(format!("Registrado: {}", list.join(", ")))
}

fn finish(result: std::io::Result<String>) -> ExitCode {
    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("w11dl-host: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Valor de `--flag <valor>` dentro de los argumentos.
fn value_of(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// `--out-dir <carpeta>`. El navegador no pasa argumentos: los fija el registro del host.
fn out_dir_from_args(args: &[String]) -> Option<PathBuf> {
    value_of(args, "--out-dir").map(PathBuf::from)
}

/// Carpeta de descargas del usuario. Si no existe la variable, la carpeta actual.
fn default_out_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%LOCALAPPDATA%\W11DownloaderPro`, o la carpeta actual si no hay variable.
fn manifest_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("W11DownloaderPro"))
        .unwrap_or_else(|| PathBuf::from("."))
}
