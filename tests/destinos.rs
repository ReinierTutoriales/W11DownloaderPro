//! Dos descargas con el mismo nombre no pueden compartir archivo, parcial ni estado, aunque ninguna
//! haya terminado todavía (en espera, en curso o interrumpidas).
mod common;

use common::{payload, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, duplex, split};
use w11dl::native::{read_message, serve as serve_host, write_message};
use w11dl::queue::{QUEUE_FILE, Record};

async fn accepted_path(read: &mut (impl tokio::io::AsyncRead + Unpin), id: &str) -> String {
    loop {
        let raw = read_message(read).await.unwrap().expect("host cerrado");
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        if msg["id"] == id && msg["type"] == "accepted" {
            return msg["path"].as_str().unwrap().to_owned();
        }
    }
}

#[tokio::test]
async fn dos_descargas_en_espera_con_el_mismo_nombre_reciben_rutas_distintas() {
    let (addr, _site) = serve(vec![("/f.zip", "application/zip", payload(1000))]).await;
    let dir = tempfile::tempdir().unwrap();
    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut read, mut write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    let mut paths = Vec::new();
    for id in ["a", "b", "c"] {
        write_message(
            &mut write,
            &json!({"type": "enqueue", "id": id, "url": format!("http://{addr}/f.zip"),
                    "filename": "archivo.zip", "queued": true}),
        )
        .await
        .unwrap();
        paths.push(accepted_path(&mut read, id).await);
    }
    write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();

    let unique: std::collections::HashSet<_> = paths.iter().collect();
    assert_eq!(unique.len(), 3, "rutas repetidas: {paths:?}");
    let records: Vec<Record> =
        serde_json::from_slice(&std::fs::read(dir.path().join(QUEUE_FILE)).unwrap()).unwrap();
    let saved: std::collections::HashSet<_> = records.iter().map(|r| r.path.clone()).collect();
    assert_eq!(saved.len(), 3, "la cola guardada repite rutas: {records:?}");
}

#[tokio::test]
async fn un_parcial_de_otra_descarga_tambien_ocupa_el_nombre() {
    let (addr, _site) = serve(vec![("/f.zip", "application/zip", payload(1000))]).await;
    let dir = tempfile::tempdir().unwrap();
    // Una descarga anterior interrumpida dejó su parcial, pero aún no existe archivo.zip.
    std::fs::write(dir.path().join("archivo.zip.part"), b"parcial ajeno").unwrap();
    std::fs::write(dir.path().join("archivo.zip.part.state"), b"{}").unwrap();

    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut read, mut write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });
    write_message(
        &mut write,
        &json!({"type": "enqueue", "id": "x", "url": format!("http://{addr}/f.zip"),
                "filename": "archivo.zip", "queued": true}),
    )
    .await
    .unwrap();
    let path = accepted_path(&mut read, "x").await;
    write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();

    assert!(
        !path.ends_with("archivo.zip"),
        "reutilizó el nombre de un parcial ajeno: {path}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("archivo.zip.part")).unwrap(),
        b"parcial ajeno",
        "el parcial ajeno no se toca"
    );
}

mod ranged;

#[tokio::test]
async fn dos_descargas_a_la_misma_ruta_no_se_pisan() {
    use std::sync::Arc;
    use std::time::Duration;
    use w11dl::{Error, Options, download};

    // 8 MiB a unos 64 KiB cada 8 ms por conexión: la primera tarda lo suficiente para solaparse con la segunda.
    let data = ranged::payload(8 << 20);
    let addr = ranged::ranged(Arc::new(data.clone()), Duration::from_millis(8), 0, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("compartido.bin");
    let make = || {
        let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
        opts.connections = 4;
        opts.max_chunk = 512 * 1024;
        opts
    };

    let first = tokio::spawn(download(make()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = download(make()).await;

    assert!(
        matches!(second, Err(Error::Busy(_))),
        "la segunda debe rechazarse: {second:?}"
    );
    first.await.unwrap().expect("la primera debe terminar bien");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        data,
        "el archivo no debe haberse corrompido"
    );
}
