#![allow(dead_code)] // cada prueba usa solo parte de estos ayudantes
//! Servidor HTTP estático para pruebas: rutas fijas, rangos, `ETag` y contador de peticiones.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, RANGE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

pub struct Site {
    routes: HashMap<String, (String, Vec<u8>)>,
    /// Peticiones recibidas, incluidas las de sondeo.
    pub hits: AtomicUsize,
    /// Peticiones que llegaron con cabecera `Cookie`.
    pub with_cookie: AtomicUsize,
}

/// Arranca un servidor con las rutas dadas: `(ruta, content-type, cuerpo)`.
pub async fn serve(routes: Vec<(&str, &str, Vec<u8>)>) -> (SocketAddr, Arc<Site>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let site = Arc::new(Site {
        routes: routes
            .into_iter()
            .map(|(p, ct, body)| (p.to_owned(), (ct.to_owned(), body)))
            .collect(),
        hits: AtomicUsize::new(0),
        with_cookie: AtomicUsize::new(0),
    });

    let accepting = Arc::clone(&site);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let site = Arc::clone(&accepting);
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let site = Arc::clone(&site);
                    async move { Ok::<_, Infallible>(handle(&site, req)) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });

    (addr, site)
}

fn handle(site: &Site, req: Request<Incoming>) -> Response<Full<Bytes>> {
    site.hits.fetch_add(1, Ordering::SeqCst);
    if req.headers().contains_key(hyper::header::COOKIE) {
        site.with_cookie.fetch_add(1, Ordering::SeqCst);
    }
    // Con proxy HTTP el cliente envía la URL absoluta; solo interesa la ruta.
    let path = req.uri().path().to_owned();
    let Some((content_type, body)) = site.routes.get(&path) else {
        return Response::builder()
            .status(404)
            .body(Full::default())
            .unwrap();
    };

    let range = req
        .headers()
        .get(RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));

    match range {
        Some((a, b)) => {
            let end = b.min(body.len() - 1);
            let part = body[a..=end].to_vec();
            Response::builder()
                .status(206)
                .header(CONTENT_TYPE, content_type.as_str())
                .header(ETAG, "\"v1\"")
                .header(CONTENT_RANGE, format!("bytes {a}-{end}/{}", body.len()))
                .header(CONTENT_LENGTH, part.len())
                .body(Full::new(Bytes::from(part)))
                .unwrap()
        }
        None => Response::builder()
            .status(200)
            .header(CONTENT_TYPE, content_type.as_str())
            .header(ETAG, "\"v1\"")
            .header(ACCEPT_RANGES, "bytes")
            .header(CONTENT_LENGTH, body.len())
            .body(Full::new(Bytes::from(body.clone())))
            .unwrap(),
    }
}

pub fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect()
}
