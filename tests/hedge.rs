//! Fragmentos lentos: una conexión libre debe competir contra ellos en vez de esperar al final,
//! sin corromper el archivo ni duplicar trabajo cuando todo va bien.
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::channel::Channel;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use w11dl::{Mode, Options, download};

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

/// Cada petición de fragmento múltiplo de `slow_every` (contando desde 1) va `slowdown` veces más lenta.
async fn server(data: Arc<Vec<u8>>, slow_every: usize, slowdown: u32) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (data, counter) = (Arc::clone(&data), Arc::clone(&counter));
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (data, counter) = (Arc::clone(&data), Arc::clone(&counter));
                    async move {
                        let (start, end) = req
                            .headers()
                            .get(RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("bytes="))
                            .and_then(|v| v.split_once('-'))
                            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                            .unwrap_or((0, data.len() - 1));
                        let end: usize = end.min(data.len() - 1);
                        let probe = (start, end) == (0, 0);
                        let slow = !probe
                            && slow_every > 0
                            && counter.fetch_add(1, Ordering::SeqCst) % slow_every
                                == slow_every - 1;
                        let (mut tx, body) = Channel::<Bytes, Infallible>::new(2);
                        let piece = data[start..=end].to_vec();
                        tokio::spawn(async move {
                            let pause = Duration::from_millis(4) * if slow { slowdown } else { 1 };
                            for p in piece.chunks(64 * 1024) {
                                tokio::time::sleep(pause).await;
                                if tx.send_data(Bytes::copy_from_slice(p)).await.is_err() {
                                    break;
                                }
                            }
                        });
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(206)
                                .header(ETAG, "\"v1\"")
                                .header(ACCEPT_RANGES, "bytes")
                                .header(
                                    CONTENT_RANGE,
                                    format!("bytes {start}-{end}/{}", data.len()),
                                )
                                .header(CONTENT_LENGTH, end - start + 1)
                                .body(body)
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

async fn run(slow_every: usize) -> (Duration, usize, bool) {
    let data = payload(16 * 1024 * 1024);
    let addr = server(Arc::new(data.clone()), slow_every, 20).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("h.bin");
    let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
    opts.connections = 8;
    opts.max_chunk = 1 << 20;
    let started = Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(60), download(opts))
        .await
        .expect("no debe colgarse")
        .unwrap();
    let elapsed = started.elapsed();
    let copies = match report.mode {
        Mode::Segmented { copies, .. } => copies,
        Mode::Single => panic!("debía ser segmentada"),
    };
    (elapsed, copies, std::fs::read(&out).unwrap() == data)
}

#[tokio::test]
async fn fragmentos_lentos_se_copian_y_el_archivo_es_identico() {
    let (slow, copies, identical) = run(6).await;
    let (clean, clean_copies, clean_identical) = run(0).await;
    eprintln!(
        "[hedge] con lentos: {slow:?} copias={copies}; sin lentos: {clean:?} copias={clean_copies}"
    );
    assert!(identical && clean_identical, "el archivo debe ser idéntico");
    assert!(copies > 0, "debía copiar al menos un fragmento lento");
    // Sin copias, un fragmento 20 veces más lento fijaría el final: el tiempo debe quedar cerca del caso limpio.
    assert!(
        slow < clean * 3,
        "con fragmentos lentos tardó {slow:?} frente a {clean:?}"
    );
}

#[tokio::test]
async fn sin_fragmentos_lentos_no_duplica_trabajo() {
    let (_, copies, identical) = run(0).await;
    assert!(identical);
    assert_eq!(copies, 0, "no debe copiar fragmentos que avanzan bien");
}
