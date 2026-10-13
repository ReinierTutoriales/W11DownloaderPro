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
    let (addr, _site) = serve(vec![
        ("/v.mp4", "video/mp4", payload(200_000)),
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
