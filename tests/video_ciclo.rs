//! Ciclo de vida del vídeo con pistas separadas: pausar y cancelar durante la unión, exclusión del
//! destino durante todo el trabajo, cancelación de una unión pausada y reanudación tras reiniciar.
mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{payload, serve};
use hyper::header::HeaderMap;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, duplex, split};
use w11dl::engine::{Control, Outcome};
use w11dl::merge::{MergeJob, Track, discard, download_merged};
use w11dl::native::{read_message, serve as serve_host, write_message};
use w11dl::queue::{QUEUE_FILE, Record, State};
use w11dl::{Error, Options, download};

/// ffmpeg falso que avisa de que arrancó y se queda colgado: no escribe nada ni termina por sí solo.
fn hanging_ffmpeg(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("ffmpeg-falso.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ntouch \"$(dirname \"$0\")/ffmpeg-arranco\"\nsleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
    #[cfg(windows)]
    {
        let script = dir.join("ffmpeg-falso.cmd");
        std::fs::write(
            &script,
            "@echo off\r\necho x> \"%~dp0ffmpeg-arranco\"\r\nping -n 60 127.0.0.1 >nul\r\n",
        )
        .unwrap();
        script
    }
}

fn job(addr: std::net::SocketAddr, ffmpeg: PathBuf) -> MergeJob {
    MergeJob {
        video: Track {
            url: format!("http://{addr}/v.mp4"),
            headers: HeaderMap::new(),
        },
        audio: Track {
            url: format!("http://{addr}/a.m4a"),
            headers: HeaderMap::new(),
        },
        ffmpeg,
    }
}

async fn wait_for(path: &Path) {
    let began = Instant::now();
    while !path.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "no apareció {path:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Running {
    handle: tokio::task::JoinHandle<w11dl::Result<Outcome>>,
    control: Control,
}

async fn start_hanging_merge(dir: &Path, output: &Path) -> Running {
    start_hanging_merge_with(dir, output, payload(200_000)).await
}

async fn start_hanging_merge_with(dir: &Path, output: &Path, video: Vec<u8>) -> Running {
    let (addr, _site) = serve(vec![
        ("/v.mp4", "video/mp4", video),
        ("/a.m4a", "audio/mp4", payload(50_000)),
    ])
    .await;
    let ffmpeg = hanging_ffmpeg(dir);
    let control = Control::default();
    let handle = tokio::spawn(download_merged(
        Options::new("ignorada", output),
        job(addr, ffmpeg),
        control.clone(),
    ));
    wait_for(&dir.join("ffmpeg-arranco")).await;
    Running { handle, control }
}

#[tokio::test]
async fn cancelar_durante_la_union_termina_ffmpeg_y_no_publica_nada() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    let running = start_hanging_merge(dir.path(), &out).await;

    let t = Instant::now();
    running.control.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), running.handle)
        .await
        .expect("cancelar debe cortar ffmpeg, no esperar a que termine")
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Outcome::Canceled), "{outcome:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    for leftover in [
        "final.mp4",
        "final.mp4.video",
        "final.mp4.audio",
        "final.mp4.merging",
        "final.mp4.part",
    ] {
        assert!(!dir.path().join(leftover).exists(), "quedó {leftover}");
    }
}

#[tokio::test]
async fn pausar_durante_la_union_conserva_las_pistas_y_no_publica() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    let running = start_hanging_merge(dir.path(), &out).await;

    running.control.pause();
    let outcome = tokio::time::timeout(Duration::from_secs(10), running.handle)
        .await
        .expect("pausar debe cortar ffmpeg")
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
    assert!(!out.exists(), "no se publica una unión interrumpida");
    assert!(!dir.path().join("final.mp4.merging").exists());
    assert!(dir.path().join("final.mp4.video").exists());
    assert!(dir.path().join("final.mp4.audio").exists());
}

#[tokio::test]
async fn un_segundo_trabajo_al_mismo_destino_no_entra_mientras_dura_el_primero() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    let running = start_hanging_merge(dir.path(), &out).await;
    // La primera está en plena unión (con sus pistas completas).
    let (addr, _site) = serve(vec![
        ("/v.mp4", "video/mp4", payload(1000)),
        ("/a.m4a", "audio/mp4", payload(1000)),
    ])
    .await;

    let second = download_merged(
        Options::new("ignorada", &out),
        job(addr, "ffmpeg-inexistente".into()),
        Control::default(),
    )
    .await;
    assert!(matches!(second, Err(Error::Busy(_))), "{second:?}");

    let direct = download(Options::new(format!("http://{addr}/v.mp4"), &out)).await;
    assert!(
        matches!(direct, Err(Error::Busy(_))),
        "una descarga directa al mismo destino tampoco entra: {direct:?}"
    );
    assert!(
        dir.path().join("final.mp4.video").exists() && dir.path().join("final.mp4.audio").exists(),
        "el rechazado no toca las pistas del primero"
    );

    running.control.cancel();
    let _ = running.handle.await;
}

#[tokio::test]
async fn cancelar_una_union_pausada_borra_pistas_parciales_y_temporal() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    for name in [
        "final.mp4.video",
        "final.mp4.video.part",
        "final.mp4.video.part.state",
        "final.mp4.audio",
        "final.mp4.audio.part",
        "final.mp4.audio.part.state",
        "final.mp4.merging",
        "final.mp4.part.state",
    ] {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    discard(&out).await.unwrap();
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != ".w11dl-locks")
        .collect();
    assert!(left.is_empty(), "quedaron restos: {left:?}");
}

#[tokio::test]
async fn cancelar_comunica_los_fallos_de_borrado_y_respeta_la_exclusion() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    // Un directorio con el nombre del temporal no se puede borrar como archivo.
    std::fs::create_dir(dir.path().join("final.mp4.merging")).unwrap();
    std::fs::write(dir.path().join("final.mp4.video"), b"x").unwrap();
    let failed = discard(&out).await;
    assert!(failed.is_err(), "el fallo de borrado debe comunicarse");
    assert!(
        !dir.path().join("final.mp4.video").exists(),
        "lo demás se borra igualmente"
    );

    // Con el destino ocupado no se toca nada.
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("otro.mp4");
    std::fs::write(dir.path().join("otro.mp4.video"), b"x").unwrap();
    let _held = w11dl::engine::DestinationLock::acquire(&out).await.unwrap();
    assert!(matches!(discard(&out).await, Err(Error::Busy(_))));
    assert!(dir.path().join("otro.mp4.video").exists());
}

#[tokio::test]
async fn el_borrado_de_parciales_de_una_descarga_directa_comunica_sus_errores() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("x.bin");
    // El estado de reanudación es un directorio: no se puede borrar como archivo.
    std::fs::create_dir(dir.path().join("x.bin.part.state")).unwrap();
    std::fs::write(dir.path().join("x.bin.part"), b"datos").unwrap();
    assert!(w11dl::engine::discard_partials(&out).await.is_err());
    // Ausentes no es un fallo.
    let vacio = tempfile::tempdir().unwrap();
    w11dl::engine::discard_partials(&vacio.path().join("y.bin"))
        .await
        .unwrap();
}

#[tokio::test]
async fn reanudar_un_video_con_pistas_tras_reiniciar_nunca_baja_solo_la_imagen() {
    let (addr, site) = serve(vec![("/v.mp4", "video/mp4", payload(5000))]).await;
    let dir = tempfile::tempdir().unwrap();
    let out_file = dir.path().join("peli.mp4");
    // Cola de una sesión anterior: el enlace de la imagen sigue vigente, el del sonido no se guardó.
    let record = Record {
        id: "v1".into(),
        url: format!("http://{addr}/v.mp4"),
        path: out_file.clone(),
        state: State::Paused,
        bytes: 0,
        via_ytdlp: false,
        merged: true,
        page: Some("http://127.0.0.1:9/pagina".into()),
    };
    std::fs::write(
        dir.path().join(QUEUE_FILE),
        serde_json::to_vec(&vec![record]).unwrap(),
    )
    .unwrap();

    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut read, mut write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    write_message(&mut write, &json!({"type": "resume", "id": "v1"}))
        .await
        .unwrap();
    let verdict = loop {
        let raw = read_message(&mut read)
            .await
            .unwrap()
            .expect("host cerrado");
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        if msg["id"] == "v1" && matches!(msg["type"].as_str(), Some("error" | "done")) {
            break msg;
        }
    };
    write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();

    assert_eq!(verdict["type"], "error", "{verdict}");
    assert_eq!(
        site.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no debía pedir la imagen sin poder resolver el sonido"
    );
    assert!(!out_file.exists(), "no puede haber un «vídeo» sin sonido");
    let records: Vec<Record> =
        serde_json::from_slice(&std::fs::read(dir.path().join(QUEUE_FILE)).unwrap()).unwrap();
    assert!(
        matches!(records[0].state, State::Failed { .. }) && records[0].merged,
        "{records:?}"
    );
}

#[tokio::test]
async fn una_pista_completa_sin_manifiesto_o_con_otro_tamano_no_se_reutiliza() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    let video = payload(200_000);
    // Una pista terminada de quién sabe qué selección, sin manifiesto: no se puede comprobar.
    std::fs::write(dir.path().join("final.mp4.video"), b"otra cosa").unwrap();
    // Otra con un manifiesto ilegible (de una versión anterior).
    std::fs::write(dir.path().join("final.mp4.audio"), vec![1u8; 50_000]).unwrap();
    std::fs::write(
        dir.path().join("final.mp4.audio.done"),
        br#"{"total":49999,"validator":"etag:\"v1\""}"#,
    )
    .unwrap();

    let running = start_hanging_merge_with(dir.path(), &out, video.clone()).await;
    assert_eq!(
        std::fs::read(dir.path().join("final.mp4.video")).unwrap(),
        video,
        "la pista sin manifiesto debía descargarse de nuevo"
    );
    assert_eq!(
        std::fs::read(dir.path().join("final.mp4.audio")).unwrap(),
        payload(50_000),
        "la pista con manifiesto de otro tamaño debía descargarse de nuevo"
    );
    assert!(dir.path().join("final.mp4.video.done").exists());
    running.control.cancel();
    let _ = running.handle.await;
}

/// ffmpeg falso que termina bien: escribe el temporal (su último argumento) y sale.
fn quick_ffmpeg(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("ffmpeg-rapido.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor a; do last=\"$a\"; done\necho unido > \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
    #[cfg(windows)]
    {
        let script = dir.join("ffmpeg-rapido.cmd");
        std::fs::write(
            &script,
            "@echo off\r\nfor %%a in (%*) do set last=%%a\r\necho unido> %last%\r\n",
        )
        .unwrap();
        script
    }
}

#[tokio::test]
async fn una_pausa_que_llega_al_terminar_la_union_no_publica() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    let (addr, _site) = serve(vec![
        ("/v.mp4", "video/mp4", payload(100_000)),
        ("/a.m4a", "audio/mp4", payload(50_000)),
    ])
    .await;
    let gate = std::sync::Arc::new(w11dl::engine::Gate::default());
    let mut opts = Options::new("ignorada", &out);
    opts.gate = Some(std::sync::Arc::clone(&gate));
    let control = Control::default();
    let handle = tokio::spawn(download_merged(
        opts,
        job(addr, quick_ffmpeg(dir.path())),
        control.clone(),
    ));
    // ffmpeg ya terminó y el temporal existe; la pausa llega justo antes de publicar.
    tokio::time::timeout(Duration::from_secs(30), gate.reached.acquire())
        .await
        .expect("no se llegó a la barrera")
        .unwrap()
        .forget();
    assert!(dir.path().join("final.mp4.merging").exists());
    control.pause();
    gate.release.add_permits(1);

    let outcome = handle.await.unwrap().unwrap();
    assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
    assert!(!out.exists(), "una pausa no puede acabar publicando");
    assert!(!dir.path().join("final.mp4.merging").exists());
    assert!(dir.path().join("final.mp4.video").exists());
    assert!(dir.path().join("final.mp4.audio").exists());
}

#[tokio::test]
async fn el_archivo_de_exclusion_es_estable_y_borrar_parciales_no_pisa_una_descarga_nueva() {
    let data = payload(300_000);
    let (addr, _site) = serve(vec![("/f.bin", "application/octet-stream", data.clone())]).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("f.bin");

    let url = format!("http://{addr}/f.bin");
    let cleaner = {
        let out = out.clone();
        tokio::spawn(async move {
            let mut cleaned = 0;
            for _ in 0..200 {
                if w11dl::engine::discard_partials(&out).await.is_ok() {
                    cleaned += 1;
                }
                tokio::task::yield_now().await;
            }
            cleaned
        })
    };
    let mut done = 0;
    for round in 0..40 {
        let _ = std::fs::remove_file(&out);
        match download(Options::new(url.clone(), &out)).await {
            Ok(_) => {
                assert!(std::fs::read(&out).unwrap() == data, "ronda {round}");
                done += 1;
            }
            Err(Error::Busy(_)) => {}
            Err(e) => panic!("ronda {round}: {e}"),
        }
    }
    let _ = cleaner.await.unwrap();
    assert!(done > 0, "ninguna descarga llegó a completarse");
    let lock = dir.path().join(".w11dl-locks").join("f.bin.lock");
    assert!(lock.exists(), "el archivo de exclusión es persistente");
    assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0);
}

fn plain_job(addr: std::net::SocketAddr, video_path: &str, ffmpeg: PathBuf) -> MergeJob {
    let mut j = job(addr, ffmpeg);
    j.video.url = format!("http://{addr}{video_path}");
    j
}

#[tokio::test]
async fn mismo_tamano_y_mismo_etag_en_otro_recurso_no_es_la_misma_pista() {
    // Dos recursos distintos, igual longitud y el mismo ETag («v1» en este servidor de pruebas).
    let (a, b) = (
        payload(120_000),
        payload(120_000).into_iter().rev().collect::<Vec<u8>>(),
    );
    assert_ne!(a, b);
    let (addr, _site) = serve(vec![
        ("/v.mp4", "video/mp4", a.clone()),
        ("/w.mp4", "video/mp4", b.clone()),
        ("/a.m4a", "audio/mp4", payload(50_000)),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("final.mp4");
    let ffmpeg = hanging_ffmpeg(dir.path());
    let marker = dir.path().join("ffmpeg-arranco");

    let control = Control::default();
    let first = tokio::spawn(download_merged(
        Options::new("ignorada", &out),
        plain_job(addr, "/v.mp4", ffmpeg.clone()),
        control.clone(),
    ));
    wait_for(&marker).await;
    control.pause();
    let outcome = first.await.unwrap().unwrap();
    assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
    assert_eq!(
        std::fs::read(dir.path().join("final.mp4.video")).unwrap(),
        a
    );
    assert!(dir.path().join("final.mp4.video.done").exists());

    // Mismo destino, pero ahora la imagen seleccionada es otro recurso.
    std::fs::remove_file(&marker).unwrap();
    let control = Control::default();
    let second = tokio::spawn(download_merged(
        Options::new("ignorada", &out),
        plain_job(addr, "/w.mp4", ffmpeg),
        control.clone(),
    ));
    wait_for(&marker).await;
    assert_eq!(
        std::fs::read(dir.path().join("final.mp4.video")).unwrap(),
        b,
        "se reutilizó la imagen de otro recurso con el mismo tamaño y ETag"
    );
    control.cancel();
    let _ = second.await;
}

/// Servidor que acepta conexiones y no responde nunca.
async fn silent_server() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
async fn pausar_y_cancelar_atienden_la_consulta_de_identidad_aunque_el_servidor_calle() {
    for cancel in [true, false] {
        let addr = silent_server().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("final.mp4");
        // Pistas completas de una ejecución anterior: hay que consultar al servidor para reutilizarlas.
        std::fs::write(dir.path().join("final.mp4.video"), b"video").unwrap();
        std::fs::write(dir.path().join("final.mp4.audio"), b"audio").unwrap();
        let control = Control::default();
        let handle = tokio::spawn(download_merged(
            Options::new("ignorada", &out),
            job(addr, "ffmpeg-inexistente".into()),
            control.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let t = Instant::now();
        if cancel {
            control.cancel();
        } else {
            control.pause();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("la orden debe atenderse sin esperar al servidor")
            .unwrap()
            .unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        if cancel {
            assert!(matches!(outcome, Outcome::Canceled), "{outcome:?}");
            assert!(!dir.path().join("final.mp4.video").exists());
        } else {
            assert!(matches!(outcome, Outcome::Paused { .. }), "{outcome:?}");
            assert!(dir.path().join("final.mp4.video").exists());
        }
    }
}
