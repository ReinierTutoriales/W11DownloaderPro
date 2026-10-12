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
