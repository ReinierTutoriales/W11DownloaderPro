//! Fragmentos lentos: una conexión libre debe competir contra ellos en vez de esperar al final,
//! sin corromper el archivo ni duplicar trabajo cuando todo va bien.
mod ranged;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ranged::{payload, ranged, ranged_slow_tail};
use w11dl::{Mode, Options, download};

async fn run(slow_tail: bool) -> (Duration, usize, bool) {
    let data = payload(16 * 1024 * 1024);
    // Con lentitud, el último tramo del archivo va 200 veces más lento la primera vez que se pide
    // (800 ms por bloque de 64 KiB). Depende de qué se pide, no del orden de las peticiones: el motor
    // trocea la descarga en fragmentos de distinto tamaño y ese reparto cambia entre máquinas.
    let addr = if slow_tail {
        ranged_slow_tail(Arc::new(data.clone()), Duration::from_millis(4), 200).await
    } else {
        ranged(Arc::new(data.clone()), Duration::from_millis(4), 0, 1).await
    };
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
    // El rezagado es siempre el último tramo del archivo: con la cola ya vacía y otras conexiones libres,
    // hay un fragmento que copiar, sea cual sea la velocidad del equipo y el troceado elegido.
    let (slow, copies, identical) = run(true).await;
    let (clean, clean_copies, clean_identical) = run(false).await;
    eprintln!(
        "[hedge] con lentos: {slow:?} copias={copies}; sin lentos: {clean:?} copias={clean_copies}"
    );
    assert!(identical && clean_identical, "el archivo debe ser idéntico");
    assert!(copies > 0, "debía copiar al menos un fragmento lento");
    // Sin copia, el tramo lento (>= 1,6 s) fijaría el final: con la copia debe quedar cerca del caso limpio.
    assert!(
        slow < clean + Duration::from_millis(1200),
        "con fragmentos lentos tardó {slow:?} frente a {clean:?}"
    );
}

#[tokio::test]
async fn sin_fragmentos_lentos_no_duplica_trabajo() {
    let (_, copies, identical) = run(false).await;
    assert!(identical);
    assert_eq!(copies, 0, "no debe copiar fragmentos que avanzan bien");
}
