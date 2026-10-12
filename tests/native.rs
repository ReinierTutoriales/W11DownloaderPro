mod common;

use std::collections::HashMap;

use common::{payload, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, duplex, split};
use w11dl::native::{read_message, serve as serve_host, write_message};

async fn next_for(reader: &mut (impl tokio::io::AsyncRead + Unpin), id: &str) -> Value {
    loop {
        let raw = read_message(reader)
            .await
            .unwrap()
            .expect("el host cerró demasiado pronto");
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        if msg["type"] == "pong" || msg["id"] == id && msg["type"] != "progress" {
            return msg;
        }
    }
}

#[tokio::test]
async fn ping_enqueue_con_cookies_y_archivo_completo() {
    let data = payload(900_000);
    let (addr, site) = serve(vec![("/vid.bin", "application/octet-stream", data.clone())]).await;
    let dir = tempfile::tempdir().unwrap();

    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    write_message(&mut ext_write, &json!({"type": "ping"}))
        .await
        .unwrap();
    assert_eq!(next_for(&mut ext_read, "").await["type"], "pong");

    let headers = HashMap::from([("Cookie".to_owned(), "sesion=abc".to_owned())]);
    write_message(
        &mut ext_write,
        &json!({"type": "enqueue", "id": "1", "url": format!("http://{addr}/vid.bin"),
                "filename": "clip.bin", "headers": headers}),
    )
    .await
    .unwrap();

    let accepted = next_for(&mut ext_read, "1").await;
    assert_eq!(accepted["type"], "accepted");
    let done = next_for(&mut ext_read, "1").await;
    assert_eq!(done["type"], "done", "{done}");

    assert_eq!(std::fs::read(dir.path().join("clip.bin")).unwrap(), data);
    assert!(site.with_cookie.load(std::sync::atomic::Ordering::SeqCst) > 0);

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn cabecera_no_permitida_se_rechaza_sin_descargar() {
    let (addr, site) = serve(vec![("/a.bin", "application/octet-stream", payload(10))]).await;
    let dir = tempfile::tempdir().unwrap();

    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    let headers = HashMap::from([("X-Evil".to_owned(), "1".to_owned())]);
    write_message(
        &mut ext_write,
        &json!({"type": "enqueue", "id": "9", "url": format!("http://{addr}/a.bin"), "headers": headers}),
    )
    .await
    .unwrap();

    let reply = next_for(&mut ext_read, "9").await;
    assert_eq!(reply["type"], "error");
    assert!(reply["message"].as_str().unwrap().contains("no permitida"));
    assert_eq!(site.hits.load(std::sync::atomic::Ordering::SeqCst), 0);

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}

const VIDEO_PAGE: &str = r#"<!doctype html><html><head><title>Vídeo de prueba</title></head>
<body><video src="/clip.mp4"></video></body></html>"#;

#[tokio::test]
async fn enqueue_video_resuelve_con_ytdlp_y_descarga() {
    // El host lee W11DL_PYTHON del entorno: aquí corre en el mismo proceso de la prueba.
    if std::env::var_os("W11DL_PYTHON").is_none() {
        eprintln!("omitido: define W11DL_PYTHON con un Python que tenga yt-dlp");
        return;
    }
    let data = payload(300_000);
    let (addr, _site) = serve(vec![
        (
            "/",
            "text/html; charset=utf-8",
            VIDEO_PAGE.as_bytes().to_vec(),
        ),
        ("/clip.mp4", "video/mp4", data.clone()),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();

    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    write_message(
        &mut ext_write,
        &json!({"type": "enqueue_video", "id": "v1", "page": format!("http://{addr}/")}),
    )
    .await
    .unwrap();

    let first = next_for(&mut ext_read, "v1").await;
    assert_eq!(first["type"], "probing", "{first}");
    let accepted = next_for(&mut ext_read, "v1").await;
    assert_eq!(accepted["type"], "accepted", "{accepted}");
    let done = next_for(&mut ext_read, "v1").await;
    assert_eq!(done["type"], "done", "{done}");

    let saved = dir.path().join(
        accepted["path"]
            .as_str()
            .unwrap()
            .rsplit(['/', '\\'])
            .next()
            .unwrap(),
    );
    assert_eq!(std::fs::read(saved).unwrap(), data);

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}

#[tokio::test]
async fn enqueue_video_rechaza_urls_no_web() {
    let dir = tempfile::tempdir().unwrap();
    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut ext_read, mut ext_write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    write_message(
        &mut ext_write,
        &json!({"type": "enqueue_video", "id": "x", "page": "file:///etc/passwd"}),
    )
    .await
    .unwrap();
    let reply = next_for(&mut ext_read, "x").await;
    assert_eq!(reply["type"], "error");
    assert!(reply["message"].as_str().unwrap().contains("no web"));

    ext_write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}
