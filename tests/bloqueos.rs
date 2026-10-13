//! Un servidor que se calla no puede dejar colgada la descarga ni retrasar sin límite pausa o cancelación.
mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::payload;
use http_body_util::channel::Channel;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use w11dl::engine::{Control, Outcome, download_controlled};
use w11dl::{Options, download};

#[derive(Clone, Copy)]
enum Mode {
    /// Responde bien al sondeo y envía parte de cada fragmento; después no manda nada más.
    StallBody,
    /// Responde bien al sondeo y acepta los fragmentos sin contestar nunca.
    StallHeaders,
    /// Ni siquiera contesta al sondeo.
    NoProbe,
    /// Sin rangos: envía parte del archivo y se calla.
    StallSingle,
}

async fn server(data: Arc<Vec<u8>>, mode: Mode) -> SocketAddr {
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
                        let range = req
                            .headers()
                            .get(RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("bytes="))
                            .and_then(|v| v.split_once('-'))
                            .and_then(|(a, b)| {
                                Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                            });
                        let probe = range == Some((0, 0));
                        if matches!(mode, Mode::NoProbe)
                            || (matches!(mode, Mode::StallHeaders) && !probe)
                        {
                            std::future::pending::<()>().await;
                        }
                        let (mut tx, body) = Channel::<Bytes, Infallible>::new(2);
                        if matches!(mode, Mode::StallSingle) {
                            tokio::spawn(async move {
                                let _ = tx.send_data(Bytes::from(vec![1u8; 4096])).await;
                                // Mantiene la conexión abierta sin enviar nada más.
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                drop(tx);
                            });
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(200)
                                    .header(CONTENT_LENGTH, total)
                                    .body(body)
                                    .unwrap(),
                            );
                        }
                        let (start, end) = range.unwrap_or((0, total - 1));
                        let end = end.min(total - 1);
                        let piece = data[start..=end].to_vec();
                        let len = piece.len();
                        tokio::spawn(async move {
                            let first = (len / 4).max(1).min(len);
                            if !probe {
                                let _ = tx.send_data(Bytes::copy_from_slice(&piece[..first])).await;
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                            } else {
                                let _ = tx.send_data(Bytes::copy_from_slice(&piece)).await;
                            }
                            drop(tx);
                        });
                        Ok(Response::builder()
                            .status(206)
                            .header(ETAG, "\"v1\"")
                            .header(ACCEPT_RANGES, "bytes")
                            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
                            .header(CONTENT_LENGTH, len)
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
    addr
}

fn options(addr: SocketAddr, dir: &std::path::Path, idle: Duration) -> Options {
    let mut opts = Options::new(format!("http://{addr}/f.bin"), dir.join("b.bin"));
    opts.connections = 2;
    opts.max_chunk = 512 * 1024;
    opts.max_attempts = 2;
    opts.idle_timeout = idle;
    opts
}

/// La descarga debe acabar en error dentro de `limit`; si se cuelga, la prueba falla.
async fn must_fail_quickly(mode: Mode, limit: Duration) -> String {
    let addr = server(Arc::new(payload(2 * 1024 * 1024)), mode).await;
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let result = tokio::time::timeout(
        limit,
        download(options(addr, dir.path(), Duration::from_millis(300))),
    )
    .await
    .unwrap_or_else(|_| panic!("la descarga sigue colgada pasados {limit:?}"));
    assert!(started.elapsed() < limit);
    assert!(!dir.path().join("b.bin").exists(), "no debe publicar nada");
    result.unwrap_err().to_string()
}

#[tokio::test]
async fn servidor_que_deja_de_enviar_datos_hace_fallar_la_descarga() {
    let message = must_fail_quickly(Mode::StallBody, Duration::from_secs(15)).await;
    assert!(
        message.contains("sin actividad") || message.contains("fragmento"),
        "{message}"
    );
}

#[tokio::test]
async fn servidor_que_no_contesta_a_los_fragmentos_hace_fallar_la_descarga() {
    must_fail_quickly(Mode::StallHeaders, Duration::from_secs(15)).await;
}

#[tokio::test]
async fn servidor_que_no_contesta_al_sondeo_hace_fallar_la_descarga() {
    must_fail_quickly(Mode::NoProbe, Duration::from_secs(15)).await;
}

#[tokio::test]
async fn sin_rangos_un_servidor_que_se_calla_tambien_falla() {
    must_fail_quickly(Mode::StallSingle, Duration::from_secs(15)).await;
}

/// Pausar o cancelar debe tener efecto aunque el servidor esté callado, sin esperar al tiempo de inactividad.
async fn control_prompt(mode: Mode, cancel: bool) {
    let addr = server(Arc::new(payload(2 * 1024 * 1024)), mode).await;
    let dir = tempfile::tempdir().unwrap();
    // Inactividad larga: lo que debe cortar la espera es el control, no el tiempo.
    let opts = options(addr, dir.path(), Duration::from_secs(600));
    let control = Control::default();
    let handle = tokio::spawn(download_controlled(opts, control.clone()));
    tokio::time::sleep(Duration::from_millis(700)).await;
    let asked = Instant::now();
    if cancel {
        control.cancel();
    } else {
        control.pause();
    }
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap_or_else(|_| panic!("el control no interrumpió la espera de red"))
        .unwrap()
        .unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(3),
        "tardó {:?}",
        asked.elapsed()
    );
    match (cancel, outcome) {
        (true, Outcome::Canceled) | (false, Outcome::Paused { .. }) => {}
        (_, other) => panic!("resultado inesperado: {other:?}"),
    }
}

#[tokio::test]
async fn pausar_funciona_con_el_servidor_callado_en_mitad_de_un_fragmento() {
    control_prompt(Mode::StallBody, false).await;
}

#[tokio::test]
async fn cancelar_funciona_con_el_servidor_callado_en_mitad_de_un_fragmento() {
    control_prompt(Mode::StallBody, true).await;
}

#[tokio::test]
async fn pausar_funciona_sin_rangos_con_el_servidor_callado() {
    control_prompt(Mode::StallSingle, false).await;
}

#[tokio::test]
async fn pausar_funciona_si_el_servidor_no_contesta_a_los_fragmentos() {
    control_prompt(Mode::StallHeaders, false).await;
}
