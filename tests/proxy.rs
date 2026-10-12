mod common;

use common::{payload, serve};
use w11dl::proxy::ProxyMode;
use w11dl::{Options, download};

#[tokio::test]
async fn descarga_pasa_por_proxy_http_explicito() {
    let data = payload(600_000);
    let (proxy_addr, site) = serve(vec![(
        "/archivo.bin",
        "application/octet-stream",
        data.clone(),
    )])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("archivo.bin");
    // El host no existe: solo puede llegar a través del proxy.
    let mut opts = Options::new("http://no-existe.invalid/archivo.bin", out.clone());
    opts.proxy = ProxyMode::Url(format!("http://{proxy_addr}"));
    download(opts).await.unwrap();

    assert_eq!(std::fs::read(&out).unwrap(), data);
    assert!(site.hits.load(std::sync::atomic::Ordering::SeqCst) > 1);
}
