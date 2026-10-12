use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use clap::Parser;
use w11dl::crawl::{self, CrawlOptions};
use w11dl::engine::{DEFAULT_CONNECTIONS, DEFAULT_MAX_CHUNK};
use w11dl::proxy::ProxyMode;
use w11dl::{Mode, Options, Report, download, naming, net, video};

/// Descarga acelerada: segmentación por rangos, reanudación, verificación SHA-256,
/// proxy y rastreo de sitios para capturar archivos.
#[derive(Parser)]
#[command(name = "w11dl", version)]
struct Cli {
    /// URL http:// o https:// a descargar, o página de inicio con `--crawl`.
    url: String,

    /// Ruta del archivo final (solo descarga directa). Por defecto, el último segmento de la URL.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Conexiones simultáneas.
    #[arg(short = 'c', long, default_value_t = DEFAULT_CONNECTIONS)]
    connections: usize,

    /// Tamaño máximo de fragmento en bytes.
    #[arg(long, default_value_t = DEFAULT_MAX_CHUNK)]
    max_chunk: u64,

    /// SHA-256 esperado en hexadecimal (solo descarga directa).
    #[arg(long)]
    sha256: Option<String>,

    /// Proxy explícito, p. ej. http://proxy:8080.
    #[arg(long, conflicts_with = "no_proxy")]
    proxy: Option<String>,

    /// Conexión directa, sin usar el proxy del sistema.
    #[arg(long)]
    no_proxy: bool,

    /// Rastrea la página indicada y descarga los archivos que encuentre.
    #[arg(long)]
    crawl: bool,

    /// Extensiones a capturar en modo rastreo, separadas por comas.
    #[arg(long, value_delimiter = ',')]
    ext: Option<Vec<String>>,

    /// Profundidad máxima de páginas en modo rastreo.
    #[arg(long, default_value_t = 2)]
    depth: u32,

    /// Número máximo de páginas a visitar en modo rastreo.
    #[arg(long, default_value_t = 200)]
    max_pages: usize,

    /// Pausa entre páginas en milisegundos, en modo rastreo.
    #[arg(long, default_value_t = 500)]
    delay_ms: u64,

    /// Resuelve la página con yt-dlp y descarga el mejor vídeo directo sin DRM.
    #[arg(long, conflicts_with = "crawl")]
    video: bool,

    /// Python con yt-dlp instalado (`python -m yt_dlp`). Por defecto, `W11DL_PYTHON` o `python`.
    #[arg(long, requires = "video")]
    python: Option<PathBuf>,

    /// Cookies Netscape de la sesión del usuario. Solo se usan si se indica explícitamente.
    #[arg(long, requires = "video")]
    cookies: Option<PathBuf>,

    /// Solo lista los archivos encontrados, sin descargarlos.
    #[arg(long, requires = "crawl")]
    dry_run: bool,

    /// Carpeta de destino para los archivos rastreados.
    #[arg(long, default_value = ".", requires = "crawl")]
    out_dir: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = if cli.crawl {
        run_crawl(cli).await
    } else {
        run_single(cli).await
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_single(cli: Cli) -> Result<bool, Box<dyn std::error::Error>> {
    let mut url = cli.url.clone();
    let mut headers = Vec::new();
    let mut default_output = default_name(&cli.url);
    if cli.video {
        let opts = video::ProbeOptions {
            python: cli.python.clone().or_else(video::default_python),
            cookies: cli.cookies.clone(),
        };
        let choice = match video::probe(&cli.url, &opts).await {
            Ok(choice) => choice,
            Err(e) => {
                eprintln!("Error: {e}");
                return Ok(false);
            }
        };
        eprintln!(
            "Vídeo: {} ({})",
            choice.filename,
            choice
                .height
                .map_or("resolución desconocida".into(), |h| format!("{h}p"))
        );
        default_output = PathBuf::from(&choice.filename);
        url = choice.url;
        headers = choice.headers;
    }
    let output = cli.output.unwrap_or(default_output);
    let progress = Arc::new(AtomicU64::new(0));
    let reporter = spawn_reporter(Arc::clone(&progress));

    let mut opts = Options::new(url, output.clone());
    opts.connections = cli.connections.max(1);
    opts.max_chunk = cli.max_chunk;
    opts.sha256 = cli.sha256;
    opts.proxy = proxy_mode(&cli.proxy, cli.no_proxy);
    opts.headers = header_map(&headers);
    opts.progress = Some(Arc::clone(&progress));

    let result = download(opts).await;
    reporter.abort();
    eprintln!();

    match result {
        Ok(report) => {
            print_report(&output, &report);
            Ok(true)
        }
        Err(e) => {
            eprintln!("Error: {e}");
            Ok(false)
        }
    }
}

async fn run_crawl(cli: Cli) -> Result<bool, Box<dyn std::error::Error>> {
    let mut copts = CrawlOptions::new(cli.url);
    if let Some(exts) = cli.ext {
        copts.extensions = exts;
    }
    copts.max_depth = cli.depth;
    copts.max_pages = cli.max_pages;
    copts.delay = Duration::from_millis(cli.delay_ms);

    let proxy = proxy_mode(&cli.proxy, cli.no_proxy);
    let client = net::build_client(cli.connections.max(1), &proxy)?;
    let found = crawl::crawl(&client, &copts).await?;
    eprintln!(
        "Páginas visitadas: {}. Archivos encontrados: {}.",
        found.pages_visited,
        found.files.len()
    );

    if cli.dry_run {
        for file in &found.files {
            println!("{file}");
        }
        return Ok(true);
    }

    std::fs::create_dir_all(&cli.out_dir)?;
    let mut all_ok = true;
    for file in &found.files {
        let output = cli.out_dir.join(naming::file_name(file));
        let progress = Arc::new(AtomicU64::new(0));
        let reporter = spawn_reporter(Arc::clone(&progress));

        let mut opts = Options::new(file.to_string(), output.clone());
        opts.connections = cli.connections.max(1);
        opts.max_chunk = cli.max_chunk;
        opts.proxy = proxy.clone();
        opts.progress = Some(progress);

        let result = download(opts).await;
        reporter.abort();
        eprintln!();

        match result {
            Ok(report) => print_report(&output, &report),
            Err(e) => {
                all_ok = false;
                eprintln!("Error en {file}: {e}");
            }
        }
    }
    Ok(all_ok)
}

/// Convierte las cabeceras de yt-dlp en un mapa válido; las que no son válidas se omiten.
fn header_map(pairs: &[(String, String)]) -> hyper::header::HeaderMap {
    let mut map = hyper::header::HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(n), Ok(v)) = (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(value),
        ) {
            map.append(n, v);
        }
    }
    map
}

fn proxy_mode(explicit: &Option<String>, direct: bool) -> ProxyMode {
    match (explicit, direct) {
        (Some(url), _) => ProxyMode::Url(url.clone()),
        (None, true) => ProxyMode::Direct,
        (None, false) => ProxyMode::System,
    }
}

fn spawn_reporter(progress: Arc<AtomicU64>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        loop {
            ticker.tick().await;
            eprint!("\r{:>12} bytes", progress.load(Ordering::Relaxed));
        }
    })
}

fn print_report(output: &Path, report: &Report) {
    let mb = report.downloaded as f64 / (1024.0 * 1024.0);
    let secs = report.elapsed.as_secs_f64().max(f64::EPSILON);
    let mode = match report.mode {
        Mode::Segmented {
            chunks,
            connections,
            settled,
        } => {
            format!(
                "segmentada ({chunks} fragmentos, pico {connections}, final {settled} conexiones)"
            )
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
