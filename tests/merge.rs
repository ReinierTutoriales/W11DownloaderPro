//! Unión de imagen y sonido con ffmpeg real. Se omite si ffmpeg no está instalado.
mod common;

use std::path::Path;
use std::process::Command;

use common::{payload, serve};
use hyper::header::HeaderMap;
use w11dl::Options;
use w11dl::engine::{Control, Outcome};
use w11dl::merge::{MergeJob, Track, download_merged};

fn ffmpeg() -> Option<std::path::PathBuf> {
    let found = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("omitido: ffmpeg no está instalado");
    }
    found.then(|| "ffmpeg".into())
}

/// Genera 2 s de imagen sola y 2 s de sonido solo.
fn make_tracks(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    let video = dir.join("v.mp4");
    let audio = dir.join("a.m4a");
    let run = |args: &[&str]| {
        let out = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=d=2:s=160x120:r=15",
        "-an",
        "-c:v",
        "mpeg4",
        video.to_str().unwrap(),
    ]);
    run(&[
        "-f",
        "lavfi",
        "-i",
        "sine=d=2",
        "-vn",
        "-c:a",
        "aac",
        audio.to_str().unwrap(),
    ]);
    (std::fs::read(video).unwrap(), std::fs::read(audio).unwrap())
}

fn streams(file: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(",")
}

fn job(addr: std::net::SocketAddr) -> MergeJob {
    MergeJob {
        video: Track {
            url: format!("http://{addr}/v.mp4"),
            headers: HeaderMap::new(),
        },
        audio: Track {
            url: format!("http://{addr}/a.m4a"),
            headers: HeaderMap::new(),
        },
        ffmpeg: "ffmpeg".into(),
    }
}

#[tokio::test]
async fn une_imagen_y_sonido_en_un_solo_archivo_y_limpia_los_intermedios() {
    if ffmpeg().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (v, a) = make_tracks(dir.path());
    let (addr, _site) = serve(vec![("/v.mp4", "video/mp4", v), ("/a.m4a", "audio/mp4", a)]).await;
    let out = dir.path().join("final.mp4");

    let outcome = download_merged(
        Options::new("ignorada", &out),
        job(addr),
        Control::default(),
    )
    .await
    .unwrap();

    let Outcome::Completed(report) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(report.bytes, std::fs::metadata(&out).unwrap().len());
    assert_eq!(streams(&out), "video,audio", "debe llevar imagen y sonido");
    for leftover in ["final.mp4.video", "final.mp4.audio", "final.mp4.merging"] {
        assert!(!dir.path().join(leftover).exists(), "quedó {leftover}");
    }
}

#[tokio::test]
async fn cancelar_no_deja_archivos_y_un_fallo_de_union_conserva_las_pistas() {
    if ffmpeg().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (v, a) = make_tracks(dir.path());
    // Datos que no son un vídeo: la descarga termina, la unión falla.
    let (addr, _site) = serve(vec![
        ("/v.mp4", "video/mp4", vec![7u8; 4096]),
        ("/a.m4a", "audio/mp4", a.clone()),
    ])
    .await;
    let out = dir.path().join("malo.mp4");
    let err = download_merged(Options::new("x", &out), job(addr), Control::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ffmpeg no pudo unir"), "{err}");
    assert!(!out.exists(), "no debe publicarse un archivo a medias");
    assert!(
        dir.path().join("malo.mp4.video").exists() && dir.path().join("malo.mp4.audio").exists()
    );

    // Con las pistas buenas ya en disco, reintentar no vuelve a pedirlas al servidor.
    std::fs::write(dir.path().join("malo.mp4.video"), v).unwrap();
    let (addr2, site2) = serve(vec![
        ("/v.mp4", "video/mp4", vec![]),
        ("/a.m4a", "audio/mp4", vec![]),
    ])
    .await;
    let outcome = download_merged(Options::new("x", &out), job(addr2), Control::default())
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Completed(_)));
    assert_eq!(site2.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(streams(&out), "video,audio");

    // Cancelación previa: nada queda en disco.
    let cancel = Control::default();
    cancel.cancel();
    let out2 = dir.path().join("cancelado.mp4");
    let (addr3, _s) = serve(vec![
        ("/v.mp4", "video/mp4", payload(300_000)),
        ("/a.m4a", "audio/mp4", payload(300_000)),
    ])
    .await;
    let outcome = download_merged(Options::new("x", &out2), job(addr3), cancel)
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Canceled), "{outcome:?}");
    assert!(!out2.exists() && !dir.path().join("cancelado.mp4.video").exists());
}
