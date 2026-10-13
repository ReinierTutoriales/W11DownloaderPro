//! Fragmentos lentos: una conexión libre debe competir contra ellos en vez de esperar al final,
//! sin corromper el archivo ni duplicar trabajo cuando todo va bien.
mod ranged;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ranged::{payload, ranged};
use w11dl::{Mode, Options, download};

async fn run(slow_every: usize) -> (Duration, usize, bool) {
    let data = payload(16 * 1024 * 1024);
    let addr = ranged(
        Arc::new(data.clone()),
        Duration::from_millis(4),
        slow_every,
        20,
    )
    .await;
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
    // 16 fragmentos de 1 MiB: el 16.º (el último) es el lento. Que el lento esté al final es lo que hace
    // determinista la prueba: con la cola ya vacía y otras conexiones libres, hay un rezagado que copiar,
    // sea cual sea la velocidad del equipo.
    let (slow, copies, identical) = run(16).await;
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
