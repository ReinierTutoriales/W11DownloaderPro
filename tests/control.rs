//! Pausa, reanudación y cancelación del motor contra un servidor lento (envía a trozos con pausas).
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::channel::Channel;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use w11dl::engine::{Control, Options, Outcome, download_controlled};

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}

/// Servidor que entrega `data` a trozos de 64 KiB con una pausa entre ellos.
async fn slow(data: Arc<Vec<u8>>, step: Duration, ranges: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let data = Arc::clone(&data);
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let data = Arc::clone(&data);
                    async move {
                        let len = data.len();
                        let range = req
                            .headers()
                            .get(RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("bytes="))
                            .and_then(|v| v.split_once('-'))
                            .and_then(|(a, b)| {
                                Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                            });
                        let (start, end, partial) = match range {
                            Some((a, b)) if ranges => (a, b.min(len - 1), true),
                            _ => (0, len - 1, false),
                        };
                        let (mut tx, body) = Channel::<Bytes, Infallible>::new(2);
                        let piece_data = data[start..=end].to_vec();
                        tokio::spawn(async move {
                            for piece in piece_data.chunks(64 * 1024) {
                                tokio::time::sleep(step).await;
                                if tx.send_data(Bytes::copy_from_slice(piece)).await.is_err() {
                                    break;
                                }
                            }
                        });
                        let mut resp = Response::builder()
                            .status(if partial { 206 } else { 200 })
                            .header(ETAG, "\"v1\"")
                            .header(CONTENT_LENGTH, end - start + 1);
                        resp = if partial {
                            resp.header(CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                        } else {
                            resp.header(ACCEPT_RANGES, "none")
                        };
                        Ok::<_, Infallible>(resp.body(body).unwrap())
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

fn opts(url: String, output: std::path::PathBuf) -> Options {
    let mut o = Options::new(url, output);
    o.connections = 2;
    o.max_chunk = 256 * 1024;
    o
}

#[tokio::test]
async fn pausa_guarda_progreso_y_reanudar_termina_exacto() {
    let data = Arc::new(payload(4 * 1024 * 1024));
    let addr = slow(Arc::clone(&data), Duration::from_millis(15), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("p.bin");
    let url = format!("http://{addr}/f.bin");

    let control = Control::default();
    let run = tokio::spawn(download_controlled(
        opts(url.clone(), out.clone()),
        control.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;
    control.pause();
    let first = run.await.unwrap().unwrap();
    let Outcome::Paused { bytes } = first else {
        panic!("se esperaba pausa, llegó {first:?}")
    };
    assert!(
        bytes > 0 && bytes < data.len() as u64,
        "progreso parcial esperado: {bytes}"
    );
    assert!(!out.exists(), "no debe publicarse un archivo incompleto");

    let second = download_controlled(opts(url, out.clone()), Control::default())
        .await
        .unwrap();
    let Outcome::Completed(report) = second else {
        panic!("la reanudación debe completar: {second:?}")
    };
    assert_eq!(
        report.resumed, bytes,
        "lo ya descargado no se vuelve a bajar"
    );
    assert_eq!(std::fs::read(&out).unwrap(), *data);
}

#[tokio::test]
async fn cancelar_borra_parcial_y_estado() {
    let data = Arc::new(payload(3 * 1024 * 1024));
    let addr = slow(Arc::clone(&data), Duration::from_millis(15), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("c.bin");

    let control = Control::default();
    let run = tokio::spawn(download_controlled(
        opts(format!("http://{addr}/f.bin"), out.clone()),
        control.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    control.cancel();
    assert_eq!(run.await.unwrap().unwrap(), Outcome::Canceled);
    assert!(!out.exists());
    assert!(
        !dir.path().join("c.bin.part").exists(),
        "el parcial debe borrarse"
    );
    assert!(
        !dir.path().join("c.bin.part.state").exists(),
        "el estado debe borrarse"
    );
}

#[tokio::test]
async fn sin_rangos_la_pausa_reinicia_y_no_publica() {
    let data = Arc::new(payload(2 * 1024 * 1024));
    let addr = slow(Arc::clone(&data), Duration::from_millis(15), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("s.bin");

    let control = Control::default();
    let run = tokio::spawn(download_controlled(
        opts(format!("http://{addr}/f.bin"), out.clone()),
        control.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    control.pause();
    let outcome = run.await.unwrap().unwrap();
    assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
    assert!(
        !dir.path().join("s.bin.part").exists(),
        "sin rangos el parcial se descarta"
    );
    assert!(!out.exists());
}

// ---- Cola por el protocolo del host ----

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWriteExt, duplex, split};
use w11dl::native::{read_message, serve as serve_host, write_message};

/// Lee mensajes hasta encontrar uno de tipo `kind` para `id`, saltando el progreso.
async fn until(reader: &mut (impl AsyncRead + Unpin), id: &str, kind: &str) -> Value {
    loop {
        let raw = read_message(reader)
            .await
            .unwrap()
            .expect("el host cerró antes de tiempo");
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        if msg["type"] == "progress" {
            continue;
        }
        if msg["id"] == id && msg["type"] == kind {
            return msg;
        }
        if msg["id"] == id && msg["type"] == "error" {
            panic!("error inesperado: {msg}");
        }
    }
}

#[tokio::test]
async fn host_pausa_reanuda_y_lista() {
    let data = Arc::new(payload(4 * 1024 * 1024));
    let addr = slow(Arc::clone(&data), Duration::from_millis(15), true).await;
    let dir = tempfile::tempdir().unwrap();
    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(serve_host(host_read, host_write, out.clone()));

    let url = format!("http://{addr}/f.bin");
    write_message(
        &mut ext_write,
        &json!({"type":"enqueue","id":"p1","url":url,"filename":"p.bin"}),
    )
    .await
    .unwrap();
    until(&mut ext_read, "p1", "accepted").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    write_message(&mut ext_write, &json!({"type":"pause","id":"p1"}))
        .await
        .unwrap();
    let paused = until(&mut ext_read, "p1", "paused").await;
    assert!(paused["bytes"].as_u64().unwrap() > 0, "{paused}");

    write_message(&mut ext_write, &json!({"type":"list"}))
        .await
        .unwrap();
    let list = loop {
        let raw = read_message(&mut ext_read).await.unwrap().unwrap();
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        if msg["type"] == "queue" {
            break msg;
        }
    };
    assert_eq!(list["items"][0]["state"], "paused", "{list}");

    write_message(&mut ext_write, &json!({"type":"resume","id":"p1"}))
        .await
        .unwrap();
    until(&mut ext_read, "p1", "accepted").await;
    until(&mut ext_read, "p1", "done").await;
    assert_eq!(std::fs::read(out.join("p.bin")).unwrap(), *data);

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn host_cancelar_borra_todo() {
    let data = Arc::new(payload(3 * 1024 * 1024));
    let addr = slow(Arc::clone(&data), Duration::from_millis(15), true).await;
    let dir = tempfile::tempdir().unwrap();
    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(serve_host(host_read, host_write, out.clone()));

    write_message(&mut ext_write, &json!({"type":"enqueue","id":"c1","url":format!("http://{addr}/f.bin"),"filename":"c.bin"})).await.unwrap();
    until(&mut ext_read, "c1", "accepted").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    write_message(&mut ext_write, &json!({"type":"cancel","id":"c1"}))
        .await
        .unwrap();
    until(&mut ext_read, "c1", "canceled").await;

    assert!(!out.join("c.bin").exists());
    assert!(!out.join("c.bin.part").exists());
    assert!(!out.join("c.bin.part.state").exists());

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn cerrar_el_host_y_reanudar_al_volver() {
    let data = Arc::new(payload(4 * 1024 * 1024));
    let addr = slow(Arc::clone(&data), Duration::from_millis(15), true).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().to_path_buf();
    let url = format!("http://{addr}/f.bin");

    // Primera sesión: se descarga un rato y el host se cierra a mitad (sin pausa explícita).
    {
        let (host_side, ext_side) = duplex(1 << 20);
        let (host_read, host_write) = split(host_side);
        let (mut ext_read, mut ext_write) = split(ext_side);
        let host = tokio::spawn(serve_host(host_read, host_write, out.clone()));
        write_message(
            &mut ext_write,
            &json!({"type":"enqueue","id":"r1","url":url,"filename":"r.bin"}),
        )
        .await
        .unwrap();
        until(&mut ext_read, "r1", "accepted").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        // Cierre brusco a mitad de descarga: el proceso muere sin pausar ni guardar nada más.
        host.abort();
        let _ = host.await;
        drop(ext_read);
        drop(ext_write);
    }

    // Segunda sesión: la cola se recupera y la descarga se reanuda desde su progreso.
    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let host = tokio::spawn(serve_host(host_read, host_write, out.clone()));
    write_message(&mut ext_write, &json!({"type":"list"}))
        .await
        .unwrap();
    let list = loop {
        let raw = read_message(&mut ext_read).await.unwrap().unwrap();
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        if msg["type"] == "queue" {
            break msg;
        }
    };
    let state = list["items"][0]["state"].as_str().unwrap().to_owned();
    assert!(
        state == "interrupted" || state == "done",
        "estado tras reinicio: {state}"
    );

    if state == "interrupted" {
        write_message(&mut ext_write, &json!({"type":"resume","id":"r1"}))
            .await
            .unwrap();
        until(&mut ext_read, "r1", "accepted").await;
        until(&mut ext_read, "r1", "done").await;
    }
    assert_eq!(std::fs::read(out.join("r.bin")).unwrap(), *data);

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}
