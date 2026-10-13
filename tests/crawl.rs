mod common;

use std::time::Duration;

use common::{payload, serve};
use w11dl::crawl::{self, CrawlOptions};
use w11dl::net;
use w11dl::proxy::ProxyMode;

const INDEX: &str = r#"<html><body>
<a href="a.zip#inicio">A</a>
<a href="/sub/">Sección</a>
<a href="/secret/s.zip">Oculto</a>
<a href="http://otro.invalid/x.zip">Externo</a>
<a href="imagen.png">Imagen</a>
</body></html>"#;

const SUB: &str =
    r#"<html><body><a href="b.pdf">B</a><a href="../a.zip">Repetido</a></body></html>"#;

#[tokio::test]
async fn rastreo_respeta_robots_mismo_host_y_filtros() {
    let (addr, _site) = serve(vec![
        ("/", "text/html; charset=utf-8", INDEX.as_bytes().to_vec()),
        ("/sub/", "text/html", SUB.as_bytes().to_vec()),
        ("/a.zip", "application/zip", payload(10)),
        ("/sub/b.pdf", "application/pdf", payload(10)),
        ("/secret/s.zip", "application/zip", payload(10)),
        (
            "/robots.txt",
            "text/plain",
            b"User-agent: *\nDisallow: /secret/\n".to_vec(),
        ),
    ])
    .await;

    let client = net::build_client(4, &ProxyMode::Direct).unwrap();
    let mut opts = CrawlOptions::new(format!("http://{addr}/"));
    opts.delay = Duration::ZERO;
    let found = crawl::crawl(&client, &opts).await.unwrap();

    let files: Vec<String> = found.files.iter().map(|u| u.path().to_owned()).collect();
    assert_eq!(files, vec!["/a.zip", "/sub/b.pdf"], "{files:?}");
    assert_eq!(found.pages_visited, 2);
}

#[tokio::test]
async fn rastreo_respeta_limite_de_paginas() {
    let (addr, _site) = serve(vec![
        ("/", "text/html", INDEX.as_bytes().to_vec()),
        ("/sub/", "text/html", SUB.as_bytes().to_vec()),
        ("/robots.txt", "text/plain", Vec::new()),
    ])
    .await;

    let client = net::build_client(4, &ProxyMode::Direct).unwrap();
    let mut opts = CrawlOptions::new(format!("http://{addr}/"));
    opts.delay = Duration::ZERO;
    opts.max_pages = 1;
    let found = crawl::crawl(&client, &opts).await.unwrap();

    assert_eq!(found.pages_visited, 1);
}
