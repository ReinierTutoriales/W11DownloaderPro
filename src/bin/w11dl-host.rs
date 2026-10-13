//! Host de mensajería nativa: lo lanza el navegador y recibe las descargas capturadas.
//!
//! Uso:
//! - Sin argumentos: modo servicio (lo ejecuta el navegador).
//! - `register [--extension-id <id>] [--firefox-id <id>]`: escribe los manifiestos y los registra.
//! - `unregister`: quita el registro.
//! - `setup`: crea el entorno de yt-dlp (Python 3.10+ y yt-dlp fijado por hash).
//! - `run-queue [--after nothing|sleep|hibernate|shutdown] [--out-dir <carpeta>]`: descarga lo que
//!   esté en cola y, si todo salió bien, ejecuta la acción final.
//! - `schedule --at AAAA-MM-DDTHH:MM [--after ...] [--out-dir <carpeta>]` / `unschedule`:
//!   crea o quita la tarea del Programador de Windows que lanza `run-queue`.
//! - `status`: indica si el entorno de vídeo está listo.

use std::path::PathBuf;
use std::process::ExitCode;

use w11dl::native;
use w11dl::register;
use w11dl::runtime;
use w11dl::schedule;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("register") => finish(run_register(&args)),
        Some("setup") => finish(
            runtime::setup(&runtime::data_dir())
                .map(|st| st.message)
                .map_err(|e| std::io::Error::other(e.to_string())),
        ),
        Some("status") => {
            let st = runtime::check(&runtime::data_dir());
            finish(Ok(st.message))
        }
        Some("run-queue") => run_queue(&args).await,
        Some("schedule") => finish(run_schedule(&args)),
        Some("unschedule") => finish(run_unschedule()),
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

async fn run_queue(args: &[String]) -> ExitCode {
    let after = match value_of(args, "--after")
        .as_deref()
        .map(schedule::After::parse)
    {
        None => schedule::After::Nothing,
        Some(Some(after)) => after,
        Some(None) => {
            eprintln!("w11dl-host: --after debe ser nothing, sleep, hibernate o shutdown");
            return ExitCode::FAILURE;
        }
    };
    let out_dir = out_dir_from_args(args).unwrap_or_else(default_out_dir);
    let summary = match native::run_pending(out_dir).await {
        Ok(summary) => summary,
        Err(e) => {
            // No es un fallo de las descargas: la cola está en uso. Sin acción final y con un código propio.
            eprintln!("w11dl-host: {e}. Se reintentará cuando esa instancia se cierre.");
            return ExitCode::from(2);
        }
    };
    println!(
        "Cola terminada: {} completadas, {} con error, {} sin terminar.",
        summary.done, summary.failed, summary.canceled
    );
    if schedule::should_run_after(&summary)
        && let Some((program, program_args)) = after.command()
        && let Err(e) = std::process::Command::new(program)
            .args(program_args)
            .status()
    {
        eprintln!("w11dl-host: no se pudo ejecutar {program}: {e}");
        return ExitCode::FAILURE;
    }
    if summary.failed == 0 && summary.canceled == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_schedule(args: &[String]) -> std::io::Result<String> {
    let invalid = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_owned());
    let when = value_of(args, "--at")
        .and_then(|t| schedule::parse_when(&t))
        .ok_or_else(|| invalid("--at debe tener el formato AAAA-MM-DDTHH:MM (hora local)"))?;
    let after = match value_of(args, "--after") {
        None => schedule::After::Nothing,
        Some(text) => schedule::After::parse(&text)
            .ok_or_else(|| invalid("--after debe ser nothing, sleep, hibernate o shutdown"))?,
    };
    let out_dir = out_dir_from_args(args).unwrap_or_else(default_out_dir);
    let xml = schedule::task_xml(&when, &std::env::current_exe()?, &out_dir, after);

    // schtasks espera el XML en UTF-16 con marca de orden de bytes.
    let file = std::env::temp_dir().join("w11dl-tarea.xml");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&file, bytes)?;
    let status = std::process::Command::new("schtasks")
        .args(["/Create", "/TN", schedule::TASK_NAME, "/XML"])
        .arg(&file)
        .arg("/F")
        .status();
    let _ = std::fs::remove_file(&file);
    match status {
        Ok(s) if s.success() => Ok(format!(
            "Programado para {when}. La cola se ejecutará aunque el navegador esté cerrado."
        )),
        Ok(s) => Err(std::io::Error::other(format!("schtasks terminó con {s}"))),
        Err(e) => Err(std::io::Error::other(format!(
            "schtasks no está disponible (solo Windows): {e}"
        ))),
    }
}

fn run_unschedule() -> std::io::Result<String> {
    let status = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", schedule::TASK_NAME, "/F"])
        .status()
        .map_err(|e| std::io::Error::other(format!("schtasks no está disponible: {e}")))?;
    if status.success() {
        Ok("Programación eliminada.".into())
    } else {
        Err(std::io::Error::other(format!(
            "schtasks terminó con {status}"
        )))
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
    // La instalación se completa aunque falte una dependencia: el vídeo queda deshabilitado y se explica por qué.
    let video = match runtime::setup(&runtime::data_dir()) {
        Ok(st) => format!("Vídeo: {}", st.message),
        Err(e) => format!("Vídeo no disponible (la descarga normal funciona igual): {e}"),
    };
    Ok(format!("Registrado: {}\n{video}", list.join(", ")))
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
