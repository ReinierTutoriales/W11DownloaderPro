//! Un servidor que responde 206 con la longitud correcta pero con bytes de otra posición, sin
//! Content-Range o con otro tamaño total no puede dar por buenos los fragmentos ni publicar el archivo,
//! aunque el usuario no haya pedido verificar un SHA-256.
mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use common::payload;
use http_body_util::Full;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use w11dl::{Options, download};

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// Entrega los bytes anteriores en un byte (misma longitud) y lo declara en Content-Range.
    Shifted,
    NoContentRange,
    WrongTotal,
    /// Declara el rango pedido pero entrega menos de lo pedido con otra cabecera de longitud.
    WrongStart,
}

async fn server(data: Arc<Vec<u8>>, fault: Fault) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let data = Arc::clone(&data);
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let data = Arc::clone(&data);
                    async move {
                        let total = data.len();
                        let (start, end) = req
                            .headers()
                            .get(RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("bytes="))
                            .and_then(|v| v.split_once('-'))
                            .and_then(|(a, b)| {
                                Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                            })
                            .unwrap_or((0, total - 1));
                        let end = end.min(total - 1);
                        let probe = (start, end) == (0, 0);
                        let mut builder = Response::builder()
                            .status(206)
                            .header(ETAG, "\"v1\"")
                            .header(ACCEPT_RANGES, "bytes");
                        let (from, to, header) = match fault {
                            Fault::Shifted if !probe && start > 0 => (
                                start - 1,
                                end - 1,
                                Some(format!("bytes {}-{}/{total}", start - 1, end - 1)),
                            ),
                            Fault::NoContentRange if !probe => (start, end, None),
                            Fault::WrongTotal if !probe => (
                                start,
                                end,
                                Some(format!("bytes {start}-{end}/{}", total + 1)),
                            ),
                            Fault::WrongStart if !probe && start > 0 => (
                                start,
                                end,
                                Some(format!("bytes {}-{end}/{total}", start - 1)),
                            ),
                            _ => (start, end, Some(format!("bytes {start}-{end}/{total}"))),
                        };
                        if let Some(h) = header {
                            builder = builder.header(CONTENT_RANGE, h);
                        }
                        let body = data[from..=to].to_vec();
                        Ok::<_, Infallible>(
                            builder
                                .header(CONTENT_LENGTH, body.len())
                                .body(Full::new(Bytes::from(body)))
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

async fn run(fault: Fault) -> (Result<(), String>, bool, Vec<u8>, Vec<u8>) {
    let data = payload(3 * 1024 * 1024);
    let addr = server(Arc::new(data.clone()), fault).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("r.bin");
    let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
    opts.connections = 4;
    opts.max_chunk = 512 * 1024;
    opts.max_attempts = 2;
    let result = download(opts).await.map(|_| ()).map_err(|e| e.to_string());
    let published = std::fs::read(&out).unwrap_or_default();
    (result, out.exists(), published, data)
}

#[tokio::test]
async fn servidor_correcto_sigue_funcionando() {
    let (result, exists, published, data) = run(Fault::None).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(exists && published == data);
}

#[tokio::test]
async fn bytes_de_otra_posicion_no_se_aceptan() {
    let (result, exists, published, data) = run(Fault::Shifted).await;
    assert!(result.is_err(), "aceptó fragmentos de otra posición");
    assert!(!exists, "se publicó un archivo corrupto");
    assert!(published != data);
}

#[tokio::test]
async fn sin_content_range_no_se_acepta() {
    let (result, exists, _, _) = run(Fault::NoContentRange).await;
    assert!(result.is_err(), "aceptó un 206 sin Content-Range");
    assert!(!exists);
}

#[tokio::test]
async fn otro_tamano_total_no_se_acepta() {
    let (result, exists, _, _) = run(Fault::WrongTotal).await;
    assert!(
        result.is_err(),
        "aceptó un Content-Range con otro tamaño total"
    );
    assert!(!exists);
}

#[tokio::test]
async fn content_range_que_empieza_en_otro_sitio_no_se_acepta() {
    let (result, exists, _, _) = run(Fault::WrongStart).await;
    assert!(result.is_err(), "aceptó un Content-Range con otro inicio");
    assert!(!exists);
}
