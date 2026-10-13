#![allow(dead_code)] // cada prueba usa solo parte de estos ayudantes
//! Servidor de rangos con Channel para pruebas de velocidad: pausa fija por bloque de 64 KiB y,
//! opcionalmente, un fragmento de cada N mucho más lento.
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

pub fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

/// Cada petición de fragmento múltiplo de `slow_every` (contando desde 1) va `slowdown` veces más lenta.
pub async fn ranged(
    data: Arc<Vec<u8>>,
    base_pause: Duration,
    slow_every: usize,
    slowdown: u32,
) -> SocketAddr {
    let counter = AtomicUsize::new(0);
    let decide = move |_: usize, _: usize, _: usize| {
        slow_every > 0 && counter.fetch_add(1, Ordering::SeqCst) % slow_every == slow_every - 1
    };
    ranged_with(data, base_pause, slowdown, Arc::new(decide)).await
}

/// La primera petición que incluye el último byte del archivo va `slowdown` veces más lenta; las demás
/// (también una copia del mismo fragmento) van a velocidad normal. La lentitud depende de qué parte del
/// archivo se pide, no del orden en que se emiten las peticiones ni de cómo se trocea la descarga, así
/// que el escenario es el mismo en cualquier máquina.
pub async fn ranged_slow_tail(
    data: Arc<Vec<u8>>,
    base_pause: Duration,
    slowdown: u32,
) -> SocketAddr {
    let taken = std::sync::atomic::AtomicBool::new(false);
    let decide = move |_start: usize, end: usize, len: usize| {
        end + 1 == len && !taken.swap(true, Ordering::SeqCst)
    };
    ranged_with(data, base_pause, slowdown, Arc::new(decide)).await
}

type SlowDecision = dyn Fn(usize, usize, usize) -> bool + Send + Sync;

async fn ranged_with(
    data: Arc<Vec<u8>>,
    base_pause: Duration,
    slowdown: u32,
    decide: Arc<SlowDecision>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (data, decide) = (Arc::clone(&data), Arc::clone(&decide));
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (data, decide) = (Arc::clone(&data), Arc::clone(&decide));
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
                        let slow = !probe && decide(start, end, data.len());
                        let (mut tx, body) = Channel::<Bytes, Infallible>::new(2);
                        let piece = data[start..=end].to_vec();
                        tokio::spawn(async move {
                            let pause = base_pause * if slow { slowdown } else { 1 };
                            for p in piece.chunks(64 * 1024) {
                                if !pause.is_zero() {
                                    tokio::time::sleep(pause).await;
                                }
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
