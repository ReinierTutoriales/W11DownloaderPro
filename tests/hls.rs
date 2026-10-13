//! Descarga delegada en yt-dlp: un HLS local generado con ffmpeg real, resuelto y bajado por yt-dlp real.
//! Requiere `W11DL_PYTHON` (con yt-dlp) y ffmpeg; si falta alguno, se omite.
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::serve;
use w11dl::engine::{Control, Outcome};
use w11dl::video::{self, ProbeOptions};
use w11dl::ytdlp::{self, YtDlpJob};

async fn tools() -> Option<(PathBuf, PathBuf)> {
    let py = std::env::var_os("W11DL_PYTHON").map(PathBuf::from);
    let ffmpeg = video::find_ffmpeg().await;
    if py.is_none() || ffmpeg.is_none() {
        eprintln!("omitido: hace falta W11DL_PYTHON y ffmpeg");
        return None;
    }
    Some((py?, ffmpeg?))
}

/// HLS de 4 s (4 fragmentos, H.264 + AAC) guardado en `tests/fixtures/hls`: no depende de que el
/// ffmpeg disponible incluya un codificador H.264 (las compilaciones LGPL no lo traen).
fn make_hls(_dir: &Path) -> Vec<(String, &'static str, Vec<u8>)> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hls");
    let mut routes = vec![(
        "/index.m3u8".to_owned(),
        "application/vnd.apple.mpegurl",
        std::fs::read(fixtures.join("index.m3u8")).unwrap(),
    )];
    for entry in std::fs::read_dir(&fixtures).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".ts") {
            routes.push((
                format!("/{name}"),
                "video/mp2t",
                std::fs::read(entry.path()).unwrap(),
            ));
        }
    }
    routes
}

/// Tipos de pista y contenedor real del archivo: "mp4:video,audio". Un TS con nombre .mp4 no pasa.
fn streams(file: &Path) -> String {
    let probe = |entries: &str| {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
            .arg(file)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let format = probe("format=format_name");
    let kinds = probe("stream=codec_type")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(",");
    let container = if format.contains("mp4") {
        "mp4"
    } else {
        "otro"
    };
    format!("{container}:{kinds}")
}

#[tokio::test]
async fn hls_local_se_resuelve_con_ytdlp_y_se_descarga_con_imagen_y_sonido() {
    let Some((py, ffmpeg)) = tools().await else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let routes = make_hls(work.path());
    let borrowed: Vec<(&str, &str, Vec<u8>)> = routes
        .iter()
        .map(|(p, t, b)| (p.as_str(), *t, b.clone()))
        .collect();
    let (addr, _site) = serve(borrowed).await;
    let page = format!("http://{addr}/index.m3u8");

    let probe = ProbeOptions {
        python: Some(py.clone()),
        cookies: None,
        ffmpeg: Some(ffmpeg.clone()),
    };
    let choice = video::probe(&page, &probe)
        .await
        .expect("yt-dlp debe resolver el HLS");
    assert!(choice.via_ytdlp, "el HLS lo cubre yt-dlp: {choice:?}");

    let out_dir = tempfile::tempdir().unwrap();
    let job = YtDlpJob {
        python: py,
        ffmpeg: Some(ffmpeg),
        page,
        out_dir: out_dir.path().to_path_buf(),
        stem: "hls local".into(),
        temp_dir: ytdlp::temp_dir_for(out_dir.path(), "t1"),
        rate_limit: 0,
        cookies: None,
    };
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (outcome, path) = ytdlp::download(&job, Control::default(), Some(progress.clone()))
        .await
        .expect("la descarga delegada debe terminar");

    let Outcome::Completed(report) = outcome else {
        panic!("{outcome:?}")
    };
    let path = path.expect("ruta final");
    assert!(
        path.starts_with(out_dir.path()) && path.is_file(),
        "{path:?}"
    );
    assert_eq!(report.bytes, std::fs::metadata(&path).unwrap().len());
    assert_eq!(
        streams(&path),
        "mp4:video,audio",
        "MP4 real con imagen y sonido"
    );
    assert!(
        !job.temp_dir.exists(),
        "la carpeta de intermedios debe borrarse"
    );
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::Relaxed),
        report.bytes
    );
}

#[tokio::test]
async fn cancelar_borra_los_intermedios_y_pausar_permite_retomar() {
    let Some((py, ffmpeg)) = tools().await else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let routes = make_hls(work.path());
    let borrowed: Vec<(&str, &str, Vec<u8>)> = routes
        .iter()
        .map(|(p, t, b)| (p.as_str(), *t, b.clone()))
        .collect();
    let (addr, _site) = serve(borrowed).await;
    let out_dir = tempfile::tempdir().unwrap();
    let job = YtDlpJob {
        python: py,
        ffmpeg: Some(ffmpeg),
        page: format!("http://{addr}/index.m3u8"),
        out_dir: out_dir.path().to_path_buf(),
        stem: "cortado".into(),
        temp_dir: ytdlp::temp_dir_for(out_dir.path(), "t2"),
        rate_limit: 0,
        cookies: None,
    };

    // Cancelación inmediata: no queda nada.
    let control = Control::default();
    let canceller = control.clone();
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        canceller.cancel();
    });
    let (outcome, path) = ytdlp::download(&job, control, None).await.unwrap();
    cancel.await.unwrap();
    // yt-dlp puede terminar antes de 300 ms; ambos desenlaces son válidos y ninguno deja restos.
    match outcome {
        Outcome::Canceled => {
            assert!(path.is_none() && !job.temp_dir.exists());
            let left: Vec<_> = std::fs::read_dir(out_dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert!(left.is_empty(), "restos tras cancelar: {left:?}");
        }
        Outcome::Completed(_) => assert!(path.unwrap().is_file()),
        other => panic!("{other:?}"),
    }

    // Pausa y reanudación: el segundo intento completa el archivo.
    for f in std::fs::read_dir(out_dir.path()).unwrap().flatten() {
        let _ = std::fs::remove_file(f.path());
    }
    let control = Control::default();
    let pauser = control.clone();
    let pause = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        pauser.pause();
    });
    let (first, _) = ytdlp::download(&job, control, None).await.unwrap();
    pause.await.unwrap();
    let finished = match first {
        Outcome::Paused { .. } => {
            let (second, path) = ytdlp::download(&job, Control::default(), None)
                .await
                .unwrap();
            assert!(matches!(second, Outcome::Completed(_)), "{second:?}");
            path.unwrap()
        }
        Outcome::Completed(_) => std::fs::read_dir(out_dir.path())
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path(),
        other => panic!("{other:?}"),
    };
    assert_eq!(streams(&finished), "mp4:video,audio");
}

#[tokio::test]
async fn el_host_descarga_un_hls_con_ytdlp_y_deja_el_archivo_real() {
    use serde_json::{Value, json};
    use tokio::io::{AsyncWriteExt, duplex, split};
    use w11dl::native::{read_message, serve as serve_host, write_message};
    use w11dl::queue::{QUEUE_FILE, Record, State};

    let Some((py, _)) = tools().await else { return };
    // El host lee W11DL_PYTHON del entorno: aquí corre en el mismo proceso de la prueba.
    assert_eq!(
        std::env::var_os("W11DL_PYTHON").map(PathBuf::from),
        Some(py)
    );
    let work = tempfile::tempdir().unwrap();
    let routes = make_hls(work.path());
    let borrowed: Vec<(&str, &str, Vec<u8>)> = routes
        .iter()
        .map(|(p, t, b)| (p.as_str(), *t, b.clone()))
        .collect();
    let (addr, _site) = serve(borrowed).await;
    let dir = tempfile::tempdir().unwrap();

    let (host_side, ext_side) = duplex(1 << 20);
    let (host_read, host_write) = split(host_side);
    let (mut read, mut write) = split(ext_side);
    let out = dir.path().to_path_buf();
    let host = tokio::spawn(async move { serve_host(host_read, host_write, out).await });

    write_message(
        &mut write,
        &json!({"type": "enqueue_video", "id": "h1", "page": format!("http://{addr}/index.m3u8")}),
    )
    .await
    .unwrap();
    let done = loop {
        let raw = read_message(&mut read)
            .await
            .unwrap()
            .expect("host cerrado");
        let msg: Value = serde_json::from_slice(&raw).unwrap();
        match msg["type"].as_str() {
            Some("done") => break msg,
            Some("error") => panic!("{msg}"),
            _ => {}
        }
    };
    let path = PathBuf::from(done["path"].as_str().unwrap());
    assert!(path.is_file(), "el archivo final debe existir: {path:?}");
    assert_eq!(streams(&path), "mp4:video,audio");

    write.shutdown().await.unwrap();
    host.await.unwrap().unwrap();
    let records: Vec<Record> =
        serde_json::from_slice(&std::fs::read(dir.path().join(QUEUE_FILE)).unwrap()).unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].via_ytdlp && records[0].state == State::Done);
    assert_eq!(
        records[0].path, path,
        "la cola guarda la ruta real, no la prevista"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.starts_with(".w11dl-")
                && n != ".w11dl-queue.json"
                && n != ".w11dl-queue.lock"
                && n != ".w11dl-locks"
        })
        .collect();
    assert!(leftovers.is_empty(), "restos de intermedios: {leftovers:?}");
    // La carpeta de exclusión es persistente por diseño: solo contiene archivos de bloqueo vacíos.
    let locks: Vec<_> = std::fs::read_dir(dir.path().join(".w11dl-locks"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    for entry in &locks {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(name.ends_with(".lock"), "no es un bloqueo: {name}");
        assert_eq!(
            entry.metadata().unwrap().len(),
            0,
            "{name} debe estar vacío"
        );
    }
}
