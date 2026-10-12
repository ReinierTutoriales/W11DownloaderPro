//! Servidores que limitan las conexiones simultáneas y rechazan el exceso con 503 o 429.
//! El motor debe adaptarse (menos conexiones) y completar, en lugar de abortar la descarga.
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::channel::Channel;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use w11dl::{Options, download};

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

struct Stats {
    refused: AtomicUsize,
    served: AtomicUsize,
}

/// Acepta como máximo `limit` peticiones de fragmento a la vez; el resto recibe `status`.
/// El sondeo `bytes=0-0` no cuenta, como una petición cualquiera de metadatos.
async fn limited(
    data: Arc<Vec<u8>>,
    limit: usize,
    status: u16,
    revoke_after: Option<usize>,
) -> (SocketAddr, Arc<Stats>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stats = Arc::new(Stats {
        refused: AtomicUsize::new(0),
        served: AtomicUsize::new(0),
    });
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (st, inf) = (Arc::clone(&stats), Arc::clone(&in_flight));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (data, st, inf) = (Arc::clone(&data), Arc::clone(&st), Arc::clone(&inf));
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (data, st, inf) = (Arc::clone(&data), Arc::clone(&st), Arc::clone(&inf));
                    async move {
                        let range = req
                            .headers()
                            .get(RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("bytes="))
                            .and_then(|v| v.split_once('-'))
                            .and_then(|(a, b)| {
                                Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                            });
                        let len = data.len();
                        let (start, end) = range
                            .map(|(a, b)| (a, b.min(len - 1)))
                            .unwrap_or((0, len - 1));
                        let probe = (start, end) == (0, 0);
                        if !probe {
                            let now = inf.fetch_add(1, Ordering::SeqCst) + 1;
                            // Enlace revocado: tras servir cierto número de fragmentos, todo es 403.
                            let revoked =
                                revoke_after.is_some_and(|n| st.served.load(Ordering::SeqCst) >= n);
                            if now > limit || revoked {
                                inf.fetch_sub(1, Ordering::SeqCst);
                                st.refused.fetch_add(1, Ordering::SeqCst);
                                let status = if revoked { 403 } else { status };
                                let (_tx, body) = Channel::<Bytes, Infallible>::new(1);
                                return Ok::<_, Infallible>(
                                    Response::builder().status(status).body(body).unwrap(),
                                );
                            }
                            st.served.fetch_add(1, Ordering::SeqCst);
                        }
                        let (mut tx, body) = Channel::<Bytes, Infallible>::new(2);
                        let piece = data[start..=end].to_vec();
                        let inf2 = Arc::clone(&inf);
                        tokio::spawn(async move {
                            for p in piece.chunks(64 * 1024) {
                                tokio::time::sleep(Duration::from_millis(8)).await;
                                if tx.send_data(Bytes::copy_from_slice(p)).await.is_err() {
                                    break;
                                }
                            }
                            if !probe {
                                inf2.fetch_sub(1, Ordering::SeqCst);
                            }
                        });
                        Ok(Response::builder()
                            .status(206)
                            .header(ETAG, "\"v1\"")
                            .header(ACCEPT_RANGES, "bytes")
                            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                            .header(CONTENT_LENGTH, end - start + 1)
                            .body(body)
                            .unwrap())
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (addr, stats)
}

async fn run(
    status: u16,
) -> (
    Result<(), String>,
    Arc<Stats>,
    Vec<u8>,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let data = payload(6 * 1024 * 1024);
    let (addr, stats) = limited(Arc::new(data.clone()), 1, status, None).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("l.bin");
    let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
    opts.connections = 8;
    opts.max_chunk = 256 * 1024;
    let started = std::time::Instant::now();
    let result = download(opts).await.map(|_| ()).map_err(|e| e.to_string());
    let elapsed = started.elapsed();
    eprintln!(
        "[refusal {status}] tiempo={:.2}s servidas={} rechazadas={}",
        started.elapsed().as_secs_f64(),
        stats.served.load(Ordering::SeqCst),
        stats.refused.load(Ordering::SeqCst)
    );
    assert!(
        elapsed < Duration::from_millis(1700),
        "con un solo flujo aceptado debe terminar en ~1 s, no esperar reintentos: {elapsed:?}"
    );
    assert!(
        stats.refused.load(Ordering::SeqCst) <= 8,
        "no debe insistir contra el servidor: {} rechazos",
        stats.refused.load(Ordering::SeqCst)
    );
    (result, stats, data, out, dir)
}

#[tokio::test]
async fn servidor_que_rechaza_con_503_el_exceso_no_aborta_la_descarga() {
    let (result, stats, data, out, _dir) = run(503).await;
    assert!(
        result.is_ok(),
        "la descarga no debe abortar: {result:?} (rechazos {})",
        stats.refused.load(Ordering::SeqCst)
    );
    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[tokio::test]
async fn servidor_que_rechaza_con_429_el_exceso_no_aborta_la_descarga() {
    let (result, stats, data, out, _dir) = run(429).await;
    assert!(
        result.is_ok(),
        "la descarga no debe abortar: {result:?} (rechazos {})",
        stats.refused.load(Ordering::SeqCst)
    );
    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[tokio::test]
async fn enlace_revocado_falla_pronto_y_no_entra_en_bucle() {
    let data = payload(6 * 1024 * 1024);
    // Sin límite de conexiones, pero tras servir 3 fragmentos todo es 403.
    let (addr, stats) = limited(Arc::new(data), 64, 403, Some(3)).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("r.bin");
    let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
    opts.connections = 4;
    opts.max_chunk = 256 * 1024;

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(20), download(opts))
        .await
        .expect("la descarga no debe colgarse con un enlace revocado");
    assert!(
        result.is_err(),
        "debe informar el error, no dar la descarga por buena"
    );
    assert!(!out.exists(), "no debe publicarse un archivo incompleto");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "tardó {:?}",
        started.elapsed()
    );
    eprintln!(
        "[revocado] tiempo={:.2}s rechazadas={}",
        started.elapsed().as_secs_f64(),
        stats.refused.load(Ordering::SeqCst)
    );
}
