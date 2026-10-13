//! La exclusión sobre el destino cubre todo el ciclo de una descarga (comprobación del estado,
//! transferencia, verificación, publicación y limpieza), y cancelar nunca deja restos propios ni toca
//! los de otra transferencia.
mod common;
mod ranged;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::CONTENT_LENGTH;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use w11dl::engine::{Control, Gate, Outcome, download_controlled};
use w11dl::limits::RateLimiter;
use w11dl::{Error, Options, download};

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn una_segunda_descarga_no_entra_mientras_la_primera_verifica_y_publica() {
    let data = ranged::payload(8 << 20);
    let addr = ranged::ranged(Arc::new(data.clone()), Duration::ZERO, 0, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("grande.bin");
    // El hash esperado se calcula una vez y las dos opciones se preparan antes de arrancar nada.
    let expected = sha256_hex(&data);
    let make = || {
        let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
        opts.connections = 4;
        opts.max_chunk = 1 << 20;
        opts.sha256 = Some(expected.clone());
        opts
    };
    let gate = Arc::new(Gate::default());
    let mut first_opts = make();
    first_opts.gate = Some(Arc::clone(&gate));
    let second_opts = make();

    let first = tokio::spawn(download(first_opts));
    // La primera se detiene con el destino bloqueado, justo antes de verificar y publicar.
    tokio::time::timeout(Duration::from_secs(60), gate.reached.acquire())
        .await
        .expect("la primera no llegó a la barrera")
        .unwrap()
        .forget();
    assert!(
        dir.path().join("grande.bin.part").exists(),
        "en la barrera el parcial completo sigue sin publicar"
    );
    assert!(!out.exists());

    let second = download(second_opts).await;
    assert!(
        matches!(second, Err(Error::Busy(_))),
        "la segunda entró entre la transferencia y la publicación: {second:?}"
    );
    assert!(
        dir.path().join("grande.bin.part").exists(),
        "la segunda rechazada no debe tocar el parcial de la primera"
    );

    gate.release.add_permits(1);
    first.await.unwrap().expect("la primera debe publicar bien");
    assert!(
        std::fs::read(&out).unwrap() == data,
        "el archivo publicado debe ser idéntico"
    );
    assert!(!dir.path().join("grande.bin.part").exists());
    assert!(!dir.path().join("grande.bin.part.state").exists());
}

/// Servidor sin rangos que responde con todo el archivo de golpe.
async fn whole_file_server(data: Arc<Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let data = Arc::clone(&data);
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<hyper::body::Incoming>| {
                    let data = Arc::clone(&data);
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header(CONTENT_LENGTH, data.len())
                                .body(Full::new(Bytes::from(data.to_vec())))
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

#[tokio::test]
async fn sin_rangos_cancelar_corta_la_espera_del_limitador() {
    let addr = whole_file_server(Arc::new(ranged::payload(4 << 20))).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = Options::new(format!("http://{addr}/f.bin"), dir.path().join("s.bin"));
    // 1 KiB/s: recibir el primer bloque de varios KiB obligaría a esperar minutos.
    opts.limiter = Some(RateLimiter::new(1024));
    let control = Control::default();
    let handle = tokio::spawn(download_controlled(opts, control.clone()));
    tokio::time::sleep(Duration::from_millis(700)).await;
    let asked = Instant::now();
    control.cancel();

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancelar no interrumpió la espera del limitador")
        .unwrap()
        .unwrap();

    assert!(matches!(outcome, Outcome::Canceled), "{outcome:?}");
    assert!(asked.elapsed() < Duration::from_secs(3));
    assert!(
        !dir.path().join("s.bin.part").exists(),
        "cancelar no debe dejar el parcial"
    );
}

/// Servidor que acepta la conexión y no contesta nunca (el sondeo se queda esperando).
async fn silent_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

#[tokio::test]
async fn cancelar_durante_el_sondeo_limpia_el_parcial_previo_y_pausar_lo_conserva() {
    let addr = silent_server().await;
    for cancel in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("previo.bin");
        let (part, state) = (
            dir.path().join("previo.bin.part"),
            dir.path().join("previo.bin.part.state"),
        );
        std::fs::write(&part, b"parcial de una ejecucion anterior").unwrap();
        std::fs::write(&state, b"{}").unwrap();
        let mut opts = Options::new(format!("http://{addr}/f.bin"), out);
        opts.idle_timeout = Duration::from_secs(600);
        let control = Control::default();
        let handle = tokio::spawn(download_controlled(opts, control.clone()));
        tokio::time::sleep(Duration::from_millis(400)).await;
        if cancel {
            control.cancel();
        } else {
            control.pause();
        }

        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("el control no interrumpió el sondeo")
            .unwrap()
            .unwrap();

        if cancel {
            assert!(matches!(outcome, Outcome::Canceled), "{outcome:?}");
            assert!(
                !part.exists() && !state.exists(),
                "cancelar debe borrar los restos de esta descarga"
            );
        } else {
            assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
            assert!(
                part.exists() && state.exists(),
                "pausar conserva el progreso"
            );
        }
    }
}

#[tokio::test]
async fn cancelar_una_descarga_rechazada_por_estar_ocupada_no_borra_lo_de_la_otra() {
    let data = ranged::payload(8 << 20);
    let addr = ranged::ranged(Arc::new(data.clone()), Duration::from_millis(8), 0, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("ocupado.bin");
    let make = || {
        let mut opts = Options::new(format!("http://{addr}/f.bin"), out.clone());
        opts.connections = 4;
        opts.max_chunk = 512 * 1024;
        opts
    };
    let first = tokio::spawn(download(make()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let control = Control::default();
    control.cancel();

    let second = download_controlled(make(), control).await;

    // Aunque la segunda esté cancelada de antemano, no es dueña del destino: no puede borrar nada.
    assert!(
        matches!(second, Err(Error::Busy(_))),
        "debía rechazarse por ocupado: {second:?}"
    );
    first
        .await
        .unwrap()
        .expect("la primera sigue y termina bien");
    assert!(std::fs::read(&out).unwrap() == data);
}

/// Descarga hasta la barrera previa a la verificación y devuelve el control y la tarea.
async fn stopped_at_gate(
    dir: &std::path::Path,
    sha: bool,
) -> (
    Control,
    Arc<Gate>,
    tokio::task::JoinHandle<w11dl::Result<Outcome>>,
    Vec<u8>,
    std::net::SocketAddr,
) {
    let data = ranged::payload(4 << 20);
    let addr = ranged::ranged(Arc::new(data.clone()), Duration::ZERO, 0, 1).await;
    let mut opts = Options::new(format!("http://{addr}/f.bin"), dir.join("f.bin"));
    opts.connections = 4;
    opts.max_chunk = 1 << 20;
    if sha {
        opts.sha256 = Some(sha256_hex(&data));
    }
    let gate = Arc::new(Gate::default());
    opts.gate = Some(Arc::clone(&gate));
    let control = Control::default();
    let handle = tokio::spawn(download_controlled(opts, control.clone()));
    gate.reached.acquire().await.unwrap().forget();
    (control, gate, handle, data, addr)
}

#[tokio::test]
async fn cancelar_al_terminar_la_transferencia_no_publica_ni_con_hash_ni_sin_el() {
    for sha in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (control, gate, handle, _data, _addr) = stopped_at_gate(dir.path(), sha).await;
        control.cancel();
        gate.release.add_permits(1);
        let outcome = handle.await.unwrap().unwrap();
        assert!(
            matches!(outcome, Outcome::Canceled),
            "sha={sha}: {outcome:?}"
        );
        assert!(!dir.path().join("f.bin").exists(), "sha={sha}: se publicó");
        assert!(!dir.path().join("f.bin.part").exists());
        assert!(!dir.path().join("f.bin.part.state").exists());
    }
}

#[tokio::test]
async fn pausar_al_terminar_la_transferencia_conserva_el_progreso_y_al_reanudar_publica() {
    for sha in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (control, gate, handle, data, addr) = stopped_at_gate(dir.path(), sha).await;
        control.pause();
        gate.release.add_permits(1);
        let outcome = handle.await.unwrap().unwrap();
        assert!(
            matches!(outcome, Outcome::Paused { .. }),
            "sha={sha}: {outcome:?}"
        );
        assert!(!dir.path().join("f.bin").exists(), "sha={sha}: se publicó");
        assert!(dir.path().join("f.bin.part").exists());

        // Al reanudar no se baja nada y se publica.
        let mut opts = Options::new(format!("http://{addr}/f.bin"), dir.path().join("f.bin"));
        opts.connections = 4;
        opts.max_chunk = 1 << 20;
        opts.sha256 = Some(sha256_hex(&data));
        let report = download(opts).await.unwrap();
        assert_eq!(report.downloaded, 0, "sha={sha}: {report:?}");
        assert!(std::fs::read(dir.path().join("f.bin")).unwrap() == data);
    }
}
