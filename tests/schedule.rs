//! Cola programada: lo puesto en espera se descarga sin navegador, y la acción final solo cuenta si todo salió bien.
mod common;

use common::{payload, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, duplex, split};
use w11dl::native::{read_message, run_pending, serve as serve_host, write_message};
use w11dl::queue::{QUEUE_FILE, Record, State};

async fn queue_two(dir: &std::path::Path, urls: &[(&str, String)]) {
    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut read, mut write) = split(ext_side);
    let out = dir.to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });
    for (id, url) in urls {
        write_message(
            &mut write,
            &json!({"type": "enqueue", "id": id, "url": url, "filename": format!("{id}.bin"), "queued": true}),
        )
        .await
        .unwrap();
        // Acusa recibo, pero no descarga.
        let raw = read_message(&mut read).await.unwrap().unwrap();
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(msg["type"], "accepted", "{msg}");
    }
    write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
}

fn records(dir: &std::path::Path) -> Vec<Record> {
    serde_json::from_slice(&std::fs::read(dir.join(QUEUE_FILE)).unwrap()).unwrap()
}

#[tokio::test]
async fn lo_puesto_en_espera_no_empieza_y_la_cola_programada_lo_descarga() {
    let a = payload(300_000);
    let b = payload(500_000);
    let (addr, site) = serve(vec![
        ("/a.bin", "application/octet-stream", a.clone()),
        ("/b.bin", "application/octet-stream", b.clone()),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    queue_two(
        dir.path(),
        &[
            ("a", format!("http://{addr}/a.bin")),
            ("b", format!("http://{addr}/b.bin")),
        ],
    )
    .await;

    // Tras cerrar el host sigue todo en espera y no se pidió nada al servidor.
    assert!(records(dir.path()).iter().all(|r| r.state == State::Queued));
    assert_eq!(site.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!dir.path().join("a.bin").exists());

    let summary = run_pending(dir.path().to_path_buf()).await;

    assert_eq!((summary.done, summary.failed, summary.canceled), (2, 0, 0));
    assert_eq!(std::fs::read(dir.path().join("a.bin")).unwrap(), a);
    assert_eq!(std::fs::read(dir.path().join("b.bin")).unwrap(), b);
    assert!(records(dir.path()).iter().all(|r| r.state == State::Done));
    assert!(w11dl::schedule::should_run_after(&summary));
}

#[tokio::test]
async fn la_cola_programada_ignora_lo_pausado_y_lo_ya_terminado() {
    let dir = tempfile::tempdir().unwrap();
    let rec = |id: &str, state: State| Record {
        id: id.into(),
        url: "http://127.0.0.1:9/nunca".into(),
        path: dir.path().join(format!("{id}.bin")),
        state,
        bytes: 0,
    };
    let all = vec![
        rec("p", State::Paused),
        rec("d", State::Done),
        rec("c", State::Canceled),
    ];
    std::fs::write(
        dir.path().join(QUEUE_FILE),
        serde_json::to_vec(&all).unwrap(),
    )
    .unwrap();

    let summary = run_pending(dir.path().to_path_buf()).await;

    assert_eq!((summary.done, summary.failed, summary.canceled), (0, 0, 0));
    assert!(
        !w11dl::schedule::should_run_after(&summary),
        "sin descargas no hay acción final"
    );
}

#[tokio::test]
async fn con_una_descarga_fallida_no_se_ejecuta_la_accion_final() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _site) = serve(vec![(
        "/ok.bin",
        "application/octet-stream",
        payload(10_000),
    )])
    .await;
    queue_two(
        dir.path(),
        &[
            ("ok", format!("http://{addr}/ok.bin")),
            ("mal", "http://127.0.0.1:9/inexistente.bin".to_owned()),
        ],
    )
    .await;

    let summary = run_pending(dir.path().to_path_buf()).await;

    assert_eq!((summary.done, summary.failed), (1, 1));
    assert!(!w11dl::schedule::should_run_after(&summary));
}
