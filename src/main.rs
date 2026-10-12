use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clap::Parser;
use w11dl::engine::{DEFAULT_CONNECTIONS, DEFAULT_MAX_CHUNK};
use w11dl::{Mode, Options, download};

/// Descarga acelerada: segmentación por rangos, reanudación y verificación SHA-256.
#[derive(Parser)]
#[command(name = "w11dl", version)]
struct Cli {
    /// URL http:// o https:// a descargar.
    url: String,

    /// Ruta del archivo final. Por defecto, el último segmento de la URL.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Conexiones simultáneas.
    #[arg(short = 'c', long, default_value_t = DEFAULT_CONNECTIONS)]
    connections: usize,

    /// Tamaño máximo de fragmento en bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_CHUNK)]
    max_chunk: u64,

    /// SHA-256 esperado en hexadecimal. El archivo solo se publica si coincide.
    #[arg(long)]
    sha256: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let output = cli.output.unwrap_or_else(|| default_name(&cli.url));
    let progress = Arc::new(AtomicU64::new(0));

    let mut opts = Options::new(cli.url, output.clone());
    opts.connections = cli.connections.max(1);
    opts.max_chunk = cli.max_chunk;
    opts.sha256 = cli.sha256;
    opts.progress = Some(Arc::clone(&progress));

    let reporter = {
        let progress = Arc::clone(&progress);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            loop {
                ticker.tick().await;
                eprint!("\r{:>12} bytes", progress.load(Ordering::Relaxed));
            }
        })
    };

    let result = download(opts).await;
    reporter.abort();
    eprintln!();

    match result {
        Ok(report) => {
            let mb = report.downloaded as f64 / (1024.0 * 1024.0);
            let secs = report.elapsed.as_secs_f64().max(f64::EPSILON);
            let mode = match report.mode {
                Mode::Segmented {
                    chunks,
                    connections,
                } => {
                    format!("segmentada ({chunks} fragmentos, {connections} conexiones)")
                }
                Mode::Single => "un solo flujo".to_owned(),
            };
            println!(
                "Listo: {} ({} bytes, {:.2} MiB/s, {mode}, reanudado {} bytes)",
                output.display(),
                report.bytes,
                mb / secs,
                report.resumed
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Último segmento de la ruta de la URL, sin query. Si no hay nombre, `descarga.bin`.
fn default_name(url: &str) -> PathBuf {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("descarga.bin");
    PathBuf::from(name)
}
