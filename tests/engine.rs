//! Pruebas de extremo a extremo contra un servidor HTTP local que simula el comportamiento real:
//! rangos, `If-Range`, redirecciones, fallos transitorios y servidores sin soporte de rangos.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LOCATION, RANGE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use w11dl::plan::{self, State, Validators};
use w11dl::{Error, Mode, Options, download};

const ETAG_VALUE: &str = "\"v1\"";

struct Server {
    data: Vec<u8>,
    ranges: bool,
    /// Número de peticiones de fragmento que se responderán con 503 antes de funcionar.
    fail_first: AtomicUsize,
    /// Peticiones de fragmento recibidas (excluye el sondeo `bytes=0-0`).
    chunk_requests: AtomicUsize,
    etag: Option<&'static str>,
    last_modified: Option<&'static str>,
}

async fn start(data: Vec<u8>, ranges: bool, fail_first: usize) -> (SocketAddr, Arc<Server>) {
    start_with(data, ranges, fail_first, Some(ETAG_VALUE), None).await
}

async fn start_with(
    data: Vec<u8>,
    ranges: bool,
    fail_first: usize,
    etag: Option<&'static str>,
    last_modified: Option<&'static str>,
) -> (SocketAddr, Arc<Server>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Arc::new(Server {
        data,
        ranges,
        fail_first: AtomicUsize::new(fail_first),
        chunk_requests: AtomicUsize::new(0),
        etag,
        last_modified,
    });

    let accepting = Arc::clone(&server);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let server = Arc::clone(&accepting);
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let server = Arc::clone(&server);
                    async move { Ok::<_, Infallible>(handle(&server, req)) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    (addr, server)
}

fn handle(server: &Server, req: Request<Incoming>) -> Response<Full<Bytes>> {
    if req.uri().path() == "/redir" {
        return Response::builder()
            .status(302)
            .header(LOCATION, "/file")
            .body(Full::default())
            .unwrap();
    }

    let range_header = req
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let is_probe = range_header.as_deref() == Some("bytes=0-0");
    if range_header.is_some() && !is_probe {
        server.chunk_requests.fetch_add(1, Ordering::SeqCst);
        if server.fail_first.load(Ordering::SeqCst) > 0 {
            server.fail_first.fetch_sub(1, Ordering::SeqCst);
            return status_only(503);
        }
    }

    let len = server.data.len() as u64;
    let range = range_header.as_deref().and_then(parse_range);
    let if_range_ok = req.headers().get(IF_RANGE).is_none_or(|v| {
        // If-Range exige coincidencia fuerte: un ETag débil nunca cuenta.
        let strong = server.etag.filter(|t| !t.starts_with("W/"));
        strong.is_some_and(|t| t.as_bytes() == v.as_bytes())
            || server
                .last_modified
                .is_some_and(|t| t.as_bytes() == v.as_bytes())
    });

    match range {
        Some((start, end)) if server.ranges && if_range_ok => {
            let end = end.min(len - 1);
            let body = server.data[start as usize..=end as usize].to_vec();
            identity(Response::builder().status(206), server)
                .header(CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(CONTENT_LENGTH, body.len())
                .body(Full::new(Bytes::from(body)))
                .unwrap()
        }
        _ => identity(Response::builder().status(200), server)
            .header(ACCEPT_RANGES, if server.ranges { "bytes" } else { "none" })
            .header(CONTENT_LENGTH, len)
            .body(Full::new(Bytes::from(server.data.clone())))
            .unwrap(),
    }
}

fn identity(
    mut builder: hyper::http::response::Builder,
    server: &Server,
) -> hyper::http::response::Builder {
    if let Some(tag) = server.etag {
        builder = builder.header(ETAG, tag);
    }
    if let Some(lm) = server.last_modified {
        builder = builder.header(hyper::header::LAST_MODIFIED, lm);
    }
    builder
}

fn status_only(code: u16) -> Response<Full<Bytes>> {
    Response::builder()
        .status(code)
        .body(Full::default())
        .unwrap()
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn options(url: String, output: std::path::PathBuf) -> Options {
    let mut opts = Options::new(url, output);
    opts.connections = 4;
    opts.max_chunk = 256 * 1024;
    opts
}

#[tokio::test]
async fn segmentada_produce_archivo_identico_al_original() {
    let data = payload(3 * 1024 * 1024 + 123);
    let (addr, server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("archivo.bin");

    let report = download(options(format!("http://{addr}/file"), out.clone()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert_eq!(report.bytes, data.len() as u64);
    assert_eq!(report.resumed, 0);
    match report.mode {
        Mode::Segmented { connections, .. } => {
            assert!(
                (1..=4).contains(&connections),
                "pico fuera de rango: {connections}"
            );
        }
        other => panic!("se esperaba descarga segmentada: {other:?}"),
    }
    assert!(server.chunk_requests.load(Ordering::SeqCst) > 1);
    assert!(!dir.path().join("archivo.bin.part").exists());
    assert!(!dir.path().join("archivo.bin.part.state").exists());
}

#[tokio::test]
async fn sin_soporte_de_rangos_usa_un_solo_flujo() {
    let data = payload(500_000);
    let (addr, _server) = start(data.clone(), false, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("simple.bin");

    let report = download(options(format!("http://{addr}/file"), out.clone()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert_eq!(report.mode, Mode::Single);
}

#[tokio::test]
async fn sigue_redirecciones_hasta_la_url_final() {
    let data = payload(100_000);
    let (addr, _server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("r.bin");

    download(options(format!("http://{addr}/redir"), out.clone()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[tokio::test]
async fn reintenta_fragmentos_con_error_transitorio() {
    let data = payload(1024 * 1024);
    let (addr, server) = start(data.clone(), true, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("fallos.bin");

    download(options(format!("http://{addr}/file"), out.clone()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert_eq!(server.fail_first.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn falla_si_el_error_no_es_transitorio_se_agota_reintentos() {
    let data = payload(512 * 1024);
    let (addr, _server) = start(data, true, usize::MAX / 2).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("agotado.bin");

    let mut opts = options(format!("http://{addr}/file"), out.clone());
    opts.max_attempts = 2;
    let err = download(opts).await.unwrap_err();

    assert!(
        matches!(err, Error::ChunkFailed { attempts: 2, .. }),
        "{err}"
    );
    assert!(!out.exists(), "no debe publicarse un archivo incompleto");
}

#[tokio::test]
async fn hash_correcto_publica_el_archivo() {
    let data = payload(700_000);
    let (addr, _server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("ok.bin");

    let mut opts = options(format!("http://{addr}/file"), out.clone());
    opts.sha256 = Some(sha256_hex(&data));
    download(opts).await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[tokio::test]
async fn hash_incorrecto_no_publica_nada() {
    let data = payload(300_000);
    let (addr, _server) = start(data, true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("malo.bin");

    let mut opts = options(format!("http://{addr}/file"), out.clone());
    opts.sha256 = Some("00".repeat(32));
    let err = download(opts).await.unwrap_err();

    assert!(matches!(err, Error::HashMismatch { .. }), "{err}");
    assert!(!out.exists());
    assert!(!dir.path().join("malo.bin.part").exists());
}

#[tokio::test]
async fn reanuda_solo_los_fragmentos_pendientes() {
    let data = payload(2 * 1024 * 1024);
    let (addr, server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("reanuda.bin");
    let part = dir.path().join("reanuda.bin.part");
    let state_path = dir.path().join("reanuda.bin.part.state");

    // Simula una descarga interrumpida: la mitad inicial ya está en disco y marcada.
    let url = format!("http://{addr}/file");
    let mut opts = options(url.clone(), out.clone());
    let chunk_size =
        plan::effective_chunk_size(data.len() as u64, opts.connections, opts.max_chunk);
    let mut state = State::new(
        &url,
        data.len() as u64,
        chunk_size,
        Validators::new(Some(ETAG_VALUE.into()), None),
    );
    let chunks = plan::plan(data.len() as u64, chunk_size);
    let half = chunks.len() / 2;
    let mut partial = vec![0u8; data.len()];
    for c in chunks.iter().take(half) {
        partial[c.start as usize..=c.end as usize]
            .copy_from_slice(&data[c.start as usize..=c.end as usize]);
        state.done[c.index] = true;
    }
    std::fs::write(&part, &partial).unwrap();
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let already = state.completed_bytes();

    opts.progress = None;
    let report = download(opts).await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert_eq!(report.resumed, already);
    assert_eq!(
        server.chunk_requests.load(Ordering::SeqCst),
        chunks.len() - half
    );
    assert!(!state_path.exists());
}

#[tokio::test]
async fn estado_corrupto_se_descarta_sin_fallar() {
    let data = payload(400_000);
    let (addr, _server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("corrupto.bin");
    std::fs::write(dir.path().join("corrupto.bin.part.state"), b"{no es json").unwrap();

    let report = download(options(format!("http://{addr}/file"), out.clone()))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert_eq!(report.resumed, 0);
}

#[tokio::test]
async fn estado_de_otro_plan_se_descarta_y_no_corrompe_el_archivo() {
    let data = payload(2 * 1024 * 1024);
    let (addr, _server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("viejo.bin");
    let url = format!("http://{addr}/file");

    // Estado "de una versión anterior": mismo recurso y tamaño de fragmento, pero un vector `done`
    // de otra longitud, con todo marcado como completo. El parcial está lleno de ceros.
    let opts = options(url.clone(), out.clone());
    let chunk_size =
        plan::effective_chunk_size(data.len() as u64, opts.connections, opts.max_chunk);
    let right_len = plan::plan(data.len() as u64, chunk_size).len();
    let mut stale = State::new(
        &url,
        data.len() as u64,
        chunk_size,
        Validators::new(Some(ETAG_VALUE.into()), None),
    );
    stale.done = vec![true; right_len + 5];
    std::fs::write(dir.path().join("viejo.bin.part"), vec![0u8; data.len()]).unwrap();
    std::fs::write(
        dir.path().join("viejo.bin.part.state"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();

    let report = download(opts).await.unwrap();

    assert_eq!(
        report.resumed, 0,
        "no debe fiarse de un estado con otro plan"
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        data,
        "el archivo publicado debe ser el correcto"
    );
}

/// Prepara un parcial con la primera mitad completada, firmado con `saved`.
fn interrupted(
    dir: &std::path::Path,
    name: &str,
    data: &[u8],
    url: &str,
    saved: Validators,
) -> (Options, usize, usize) {
    let opts = options(url.to_owned(), dir.join(name));
    let chunk_size =
        plan::effective_chunk_size(data.len() as u64, opts.connections, opts.max_chunk);
    let mut state = State::new(url, data.len() as u64, chunk_size, saved);
    let chunks = plan::plan(data.len() as u64, chunk_size);
    let half = chunks.len() / 2;
    let mut partial = vec![0u8; data.len()];
    for c in chunks.iter().take(half) {
        partial[c.start as usize..=c.end as usize]
            .copy_from_slice(&data[c.start as usize..=c.end as usize]);
        state.done[c.index] = true;
    }
    std::fs::write(dir.join(format!("{name}.part")), &partial).unwrap();
    std::fs::write(
        dir.join(format!("{name}.part.state")),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    (opts, chunks.len(), half)
}

#[tokio::test]
async fn recurso_cambiado_en_el_servidor_no_se_reanuda() {
    let data = payload(2 * 1024 * 1024);
    let (addr, server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    // El parcial es de otra versión (ETag distinto) y está lleno de ceros marcados como hechos.
    let (opts, chunks, _) = interrupted(
        dir.path(),
        "cambiado.bin",
        &data,
        &url,
        Validators::new(Some("\"v0\"".into()), None),
    );

    let report = download(opts).await.unwrap();

    assert_eq!(report.resumed, 0);
    assert_eq!(server.chunk_requests.load(Ordering::SeqCst), chunks);
    assert_eq!(
        std::fs::read(dir.path().join("cambiado.bin")).unwrap(),
        data
    );
}

#[tokio::test]
async fn servidor_sin_validadores_no_reanuda_sin_sha256() {
    let data = payload(2 * 1024 * 1024);
    let (addr, server) = start_with(data.clone(), true, 0, None, None).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    let (opts, chunks, _) =
        interrupted(dir.path(), "sinval.bin", &data, &url, Validators::default());

    let report = download(opts).await.unwrap();

    assert_eq!(
        report.resumed, 0,
        "sin validadores ni hash, no se fía del parcial"
    );
    assert_eq!(
        server.chunk_requests.load(Ordering::SeqCst),
        chunks,
        "baja todo de nuevo"
    );
    assert_eq!(std::fs::read(dir.path().join("sinval.bin")).unwrap(), data);
}

#[tokio::test]
async fn servidor_sin_validadores_reanuda_si_hay_sha256() {
    let data = payload(2 * 1024 * 1024);
    let (addr, server) = start_with(data.clone(), true, 0, None, None).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    let (mut opts, chunks, half) = interrupted(
        dir.path(),
        "conhash.bin",
        &data,
        &url,
        Validators::default(),
    );
    opts.sha256 = Some(sha256_hex(&data));

    let report = download(opts).await.unwrap();

    assert!(
        report.resumed > 0,
        "con SHA-256 el resultado se verifica: se puede reanudar"
    );
    assert_eq!(server.chunk_requests.load(Ordering::SeqCst), chunks - half);
    assert_eq!(std::fs::read(dir.path().join("conhash.bin")).unwrap(), data);
}

#[tokio::test]
async fn reanudar_sin_validadores_un_archivo_cambiado_lo_detecta_el_hash() {
    // El servidor cambió el contenido manteniendo el tamaño; el parcial viene de la versión anterior.
    let old = payload(2 * 1024 * 1024);
    let mut new = old.clone();
    for byte in new.iter_mut().take(1000) {
        *byte ^= 0xFF;
    }
    let (addr, _server) = start_with(new.clone(), true, 0, None, None).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    let (mut opts, _, _) = interrupted(
        dir.path(),
        "cambiado2.bin",
        &old,
        &url,
        Validators::default(),
    );
    opts.sha256 = Some(sha256_hex(&new));

    let err = download(opts).await.unwrap_err();

    assert!(matches!(err, Error::HashMismatch { .. }), "{err}");
    assert!(
        !dir.path().join("cambiado2.bin").exists(),
        "no se publica un archivo mezclado"
    );
}

#[tokio::test]
async fn etag_debil_usa_last_modified_en_if_range_y_reanuda() {
    let data = payload(2 * 1024 * 1024);
    let lm = "Wed, 21 Oct 2026 07:28:00 GMT";
    let (addr, server) = start_with(data.clone(), true, 0, Some("W/\"v1\""), Some(lm)).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    let saved = Validators::new(Some("W/\"v1\"".into()), Some(lm.into()));
    let (opts, chunks, half) = interrupted(dir.path(), "debil.bin", &data, &url, saved);

    let report = download(opts).await.unwrap();

    assert!(report.resumed > 0);
    assert_eq!(server.chunk_requests.load(Ordering::SeqCst), chunks - half);
    assert_eq!(std::fs::read(dir.path().join("debil.bin")).unwrap(), data);
}

#[tokio::test]
async fn parcial_truncado_con_estado_de_completados_no_se_reutiliza() {
    let data = payload(2 * 1024 * 1024);
    let (addr, server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    let (opts, chunks, _) = interrupted(
        dir.path(),
        "truncado.bin",
        &data,
        &url,
        Validators::new(Some(ETAG_VALUE.into()), None),
    );
    // El parcial perdió su final (disco lleno, copia a medias…) pero el estado sigue diciendo "hecho".
    let part = dir.path().join("truncado.bin.part");
    let file = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
    file.set_len(data.len() as u64 / 4).unwrap();

    let report = download(opts).await.unwrap();

    assert_eq!(
        report.resumed, 0,
        "no debe fiarse de un parcial de otra longitud"
    );
    assert_eq!(
        server.chunk_requests.load(Ordering::SeqCst),
        chunks,
        "baja todo de nuevo"
    );
    assert_eq!(
        std::fs::read(dir.path().join("truncado.bin")).unwrap(),
        data
    );
}

#[tokio::test]
async fn parcial_mas_largo_de_lo_debido_tampoco_se_reutiliza() {
    let data = payload(2 * 1024 * 1024);
    let (addr, _server) = start(data.clone(), true, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("http://{addr}/file");
    let (opts, _, _) = interrupted(
        dir.path(),
        "largo.bin",
        &data,
        &url,
        Validators::new(Some(ETAG_VALUE.into()), None),
    );
    let part = dir.path().join("largo.bin.part");
    let mut bytes = std::fs::read(&part).unwrap();
    bytes.extend_from_slice(b"basura al final");
    std::fs::write(&part, bytes).unwrap();

    let report = download(opts).await.unwrap();

    assert_eq!(report.resumed, 0);
    assert_eq!(std::fs::read(dir.path().join("largo.bin")).unwrap(), data);
}
