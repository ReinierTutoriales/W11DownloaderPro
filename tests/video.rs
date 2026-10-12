//! Integración con yt-dlp real. Requiere `W11DL_PYTHON` apuntando a un Python con yt-dlp instalado.
mod common;

use common::{payload, serve};
use w11dl::video::{self, ProbeOptions};
use w11dl::{Options, download};

const PAGE: &str = r#"<!doctype html><html><head><title>Demo local</title></head>
<body><video src="/clip.mp4"></video></body></html>"#;

fn python() -> Option<std::path::PathBuf> {
    let p = std::env::var_os("W11DL_PYTHON").map(std::path::PathBuf::from);
    if p.is_none() {
        eprintln!("omitido: define W11DL_PYTHON con un Python que tenga yt-dlp");
    }
    p
}

#[tokio::test]
async fn pagina_con_video_se_resuelve_y_descarga_intacta() {
    let Some(py) = python() else { return };
    let data = payload(400_000);
    let (addr, _site) = serve(vec![
        ("/", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        ("/clip.mp4", "video/mp4", data.clone()),
    ])
    .await;

    let page = format!("http://{addr}/");
    let opts = ProbeOptions {
        python: Some(py),
        cookies: None,
    };
    let choice = video::probe(&page, &opts)
        .await
        .expect("yt-dlp debe resolver el vídeo");
    assert!(choice.url.ends_with("/clip.mp4"), "{choice:?}");
    // yt-dlp puede añadir sufijos al título (p. ej. " (1)"): solo se exige el nombre base y la extensión.
    assert!(choice.filename.starts_with("Demo local"), "{choice:?}");
    assert!(choice.filename.ends_with(".mp4"), "{choice:?}");

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join(&choice.filename);
    let mut dl = Options::new(choice.url.clone(), out.clone());
    for (name, value) in &choice.headers {
        dl.headers.insert(
            hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            hyper::header::HeaderValue::from_str(value).unwrap(),
        );
    }
    download(dl).await.expect("descarga del formato elegido");
    assert_eq!(std::fs::read(&out).unwrap(), data);
}

#[tokio::test]
async fn pagina_sin_video_da_error_claro() {
    let Some(py) = python() else { return };
    let (addr, _site) = serve(vec![("/", "text/html", b"<html></html>".to_vec())]).await;
    let opts = ProbeOptions {
        python: Some(py),
        cookies: None,
    };
    let err = video::probe(&format!("http://{addr}/"), &opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("vídeo"), "{err}");
}

#[tokio::test]
async fn sin_python_configurado_el_error_indica_setup() {
    let opts = ProbeOptions {
        python: None,
        cookies: None,
    };
    let err = video::probe("http://127.0.0.1:9/", &opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("setup"), "{err}");
}
