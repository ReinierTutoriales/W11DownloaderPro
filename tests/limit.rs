//! Límite de velocidad: se respeta en descargas segmentadas y se reparte entre descargas simultáneas.
mod ranged;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ranged::{payload, ranged};
use w11dl::limits::RateLimiter;
use w11dl::{Options, download};

const MIB: u64 = 1 << 20;

async fn fetch(
    addr: std::net::SocketAddr,
    name: &str,
    limiter: Option<Arc<RateLimiter>>,
) -> (Duration, bool, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(name);
    let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
    opts.connections = 8;
    opts.max_chunk = 512 * 1024;
    opts.limiter = limiter;
    let started = Instant::now();
    download(opts).await.unwrap();
    (started.elapsed(), true, std::fs::read(&out).unwrap())
}

#[tokio::test]
async fn una_descarga_respeta_el_limite() {
    let data = payload(8 * MIB as usize);
    let addr = ranged(Arc::new(data.clone()), Duration::ZERO, 0, 1).await;

    // Sin límite, el bucle local es muy rápido: referencia.
    let (free, _, _) = fetch(addr, "libre.bin", None).await;

    let rate = 4 * MIB;
    let (limited, _, got) = fetch(addr, "lim.bin", Some(RateLimiter::new(rate))).await;
    let achieved = data.len() as f64 / limited.as_secs_f64();
    eprintln!(
        "[limite] libre={free:?} limitado={limited:?} velocidad={:.2} MiB/s (límite 4)",
        achieved / MIB as f64
    );

    assert_eq!(got, data, "el archivo debe ser idéntico");
    assert!(
        free < limited / 2,
        "la referencia sin límite debe ser mucho más rápida"
    );
    assert!(
        achieved <= rate as f64 * 1.10,
        "superó el límite: {achieved}"
    );
    assert!(
        achieved >= rate as f64 * 0.75,
        "muy por debajo del límite: {achieved}"
    );
}

#[tokio::test]
async fn un_limite_compartido_reparte_el_total_entre_descargas() {
    let data = payload(6 * MIB as usize);
    let addr = ranged(Arc::new(data.clone()), Duration::ZERO, 0, 1).await;
    let rate = 6 * MIB;
    let shared = RateLimiter::new(rate);

    let started = Instant::now();
    let (a, b) = tokio::join!(
        fetch(addr, "a.bin", Some(Arc::clone(&shared))),
        fetch(addr, "b.bin", Some(Arc::clone(&shared)))
    );
    let total = started.elapsed();
    let achieved = (2 * data.len()) as f64 / total.as_secs_f64();
    eprintln!(
        "[limite global] 2 descargas en {total:?} velocidad total={:.2} MiB/s (límite 6)",
        achieved / MIB as f64
    );

    assert_eq!(a.2, data);
    assert_eq!(b.2, data);
    assert!(
        achieved <= rate as f64 * 1.10,
        "el total superó el límite: {achieved}"
    );
    assert!(
        achieved >= rate as f64 * 0.75,
        "total muy por debajo del límite: {achieved}"
    );
}
