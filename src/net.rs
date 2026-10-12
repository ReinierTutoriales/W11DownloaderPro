//! Capa de red: cliente HTTP/1.1 y HTTP/2 con ALPN, sondeo del recurso y peticiones de rango.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::HeaderMap;
use hyper::header::{
    ACCEPT_ENCODING, CONTENT_LENGTH, ETAG, IF_RANGE, LAST_MODIFIED, LOCATION, RANGE, USER_AGENT,
};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::error::{Error, Result};
use crate::plan::Chunk;
use crate::proxy::{ProxyConnector, ProxyMode};

pub type HttpClient = Client<HttpsConnector<ProxyConnector>, Empty<Bytes>>;

const MAX_REDIRECTS: usize = 10;
const USER_AGENT_VALUE: &str = concat!("w11dl/", env!("CARGO_PKG_VERSION"));

/// Aplica cabeceras propias de la descarga (p. ej. cookies de la sesión del navegador).
/// Reemplaza por completo las cabeceras con el mismo nombre, para no duplicar `User-Agent`.
fn apply_extra<B>(req: &mut Request<B>, extra: &HeaderMap) {
    for name in extra.keys() {
        req.headers_mut().remove(name);
    }
    for (name, value) in extra {
        req.headers_mut().append(name, value.clone());
    }
}

/// Metadatos del recurso remoto tras sondearlo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// URL final tras seguir redirecciones.
    pub url: Uri,
    /// Tamaño total, si el servidor lo informa.
    pub total: Option<u64>,
    /// Si el servidor atiende peticiones `Range` con 206.
    pub ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Crea el cliente. Los certificados vienen del almacén del sistema, que en Windows es el oficial.
pub fn build_client(max_idle_per_host: usize, proxy: &ProxyMode) -> Result<HttpClient> {
    let connector = ProxyConnector::new(proxy);
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_all_versions()
        .wrap_connector(connector);

    Ok(Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(max_idle_per_host)
        .build(https))
}

/// Valida el esquema de una URL. Solo se aceptan `http` y `https`.
pub fn parse_url(raw: &str) -> Result<Uri> {
    let uri: Uri = raw.parse().map_err(|_| Error::InvalidUrl(raw.to_owned()))?;
    match uri.scheme_str() {
        Some("http") | Some("https") if uri.authority().is_some() => Ok(uri),
        Some(other) => Err(Error::UnsupportedScheme(other.to_owned())),
        None => Err(Error::InvalidUrl(raw.to_owned())),
    }
}

/// Sondea el recurso con `Range: bytes=0-0`. Sigue redirecciones manualmente
/// para conocer la URL final y evita descargar el cuerpo.
pub async fn probe(client: &HttpClient, raw: &str, extra: &HeaderMap) -> Result<Remote> {
    let mut uri = parse_url(raw)?;
    let origin = uri.authority().cloned();

    for _ in 0..=MAX_REDIRECTS {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(&uri)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, "bytes=0-0")
            .body(Empty::<Bytes>::new())
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if uri.authority() == origin.as_ref() {
            apply_extra(&mut req, extra);
        }

        let resp = client.request(req).await?;
        let status = resp.status();
        let headers = resp.headers();

        if status.is_redirection() {
            if let Some(loc) = headers.get(LOCATION).and_then(|v| v.to_str().ok()) {
                uri = resolve(&uri, loc)?;
                continue;
            }
            return Err(Error::Status(status.as_u16()));
        }

        let etag = header_string(headers, ETAG);
        let last_modified = header_string(headers, LAST_MODIFIED);

        return match status {
            StatusCode::PARTIAL_CONTENT => {
                let total = headers
                    .get(hyper::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .map(parse_content_range_total)
                    .transpose()?;
                Ok(Remote {
                    url: uri,
                    total,
                    ranges: true,
                    etag,
                    last_modified,
                })
            }
            StatusCode::OK => {
                // El servidor ignoró el rango: el recurso se bajará en un solo flujo.
                let total = headers
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok());
                Ok(Remote {
                    url: uri,
                    total,
                    ranges: false,
                    etag,
                    last_modified,
                })
            }
            other => Err(Error::Status(other.as_u16())),
        };
    }
    Err(Error::TooManyRedirects)
}

/// Descarga un fragmento completo. Usa `If-Range` cuando hay validador, para que el servidor
/// responda con datos inconsistentes (200 completo) en lugar de mezclar versiones del archivo.
///
/// Cada bloque recibido se suma a todos los contadores de `received` al llegar (no al terminar el fragmento): así la
/// velocidad medida es continua y las conexiones nuevas aportan desde el primer byte.
pub async fn fetch_chunk(
    client: &HttpClient,
    url: &Uri,
    chunk: Chunk,
    etag: Option<&str>,
    extra: &HeaderMap,
    received: &[&std::sync::atomic::AtomicU64],
) -> Result<Bytes> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, format!("bytes={}-{}", chunk.start, chunk.end));
    if let Some(tag) = etag {
        builder = builder.header(IF_RANGE, tag);
    }
    let mut req = builder
        .body(Empty::<Bytes>::new())
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;
    apply_extra(&mut req, extra);

    let resp = client.request(req).await?;
    let status = resp.status();
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(Error::RangeIgnored(status.as_u16()));
    }

    let expected = chunk.len();
    let mut body = resp.into_body();
    let mut buf = bytes::BytesMut::with_capacity(expected as usize);
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            for counter in received {
                counter.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
            buf.extend_from_slice(&data);
            // Un servidor que envía de más no debe llenar la memoria.
            if buf.len() as u64 > expected {
                return Err(Error::Truncated {
                    expected,
                    received: buf.len() as u64,
                });
            }
        }
    }
    if buf.len() as u64 != expected {
        return Err(Error::Truncated {
            expected,
            received: buf.len() as u64,
        });
    }
    Ok(buf.freeze())
}

/// Pide el recurso completo en un solo flujo, para servidores sin soporte de rangos.
pub async fn get_full(
    client: &HttpClient,
    url: &Uri,
    extra: &HeaderMap,
) -> Result<Response<Incoming>> {
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header(ACCEPT_ENCODING, "identity")
        .body(Empty::<Bytes>::new())
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;
    apply_extra(&mut req, extra);

    let resp = client.request(req).await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status().as_u16()));
    }
    Ok(resp)
}

fn header_string(headers: &hyper::HeaderMap, name: hyper::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Extrae el tamaño total de `bytes 0-0/12345`.
fn parse_content_range_total(value: &str) -> Result<u64> {
    value
        .rsplit('/')
        .next()
        .and_then(|t| t.parse::<u64>().ok())
        .ok_or_else(|| Error::BadContentRange(value.to_owned()))
}

/// Resuelve una cabecera `Location` (absoluta o relativa) contra la URL actual.
fn resolve(base: &Uri, location: &str) -> Result<Uri> {
    if let Ok(abs) = location.parse::<Uri>() {
        if abs.scheme().is_some() && abs.authority().is_some() {
            return parse_url(&abs.to_string());
        }
    }
    let scheme = base.scheme_str().unwrap_or("https");
    let authority = base.authority().map(|a| a.as_str()).unwrap_or_default();
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        let dir = base.path().rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        format!("{dir}/{location}")
    };
    parse_url(&format!("{scheme}://{authority}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_extrae_total() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345").unwrap(), 12345);
        assert!(parse_content_range_total("bytes 0-0/*").is_err());
    }

    #[test]
    fn redireccion_relativa_se_resuelve_contra_la_base() {
        let base: Uri = "https://ejemplo.test/a/b.zip".parse().unwrap();
        assert_eq!(
            resolve(&base, "/c.zip").unwrap().to_string(),
            "https://ejemplo.test/c.zip"
        );
        assert_eq!(
            resolve(&base, "d.zip").unwrap().to_string(),
            "https://ejemplo.test/a/d.zip"
        );
        assert_eq!(
            resolve(&base, "http://otro.test/x").unwrap().to_string(),
            "http://otro.test/x"
        );
    }

    #[test]
    fn solo_acepta_http_y_https() {
        assert!(parse_url("ftp://x.test/a").is_err());
        assert!(parse_url("file:///etc/passwd").is_err());
        assert!(parse_url("https://x.test/a").is_ok());
    }
}

/// Cuerpo pequeño de una respuesta, con su tipo de contenido.
#[derive(Debug, Clone)]
pub struct Body {
    pub content_type: Option<String>,
    pub bytes: Bytes,
}

/// Descarga un recurso pequeño entero (páginas HTML, `robots.txt`).
/// Devuelve `None` si no existe (404/410) o si supera `limit` bytes.
pub async fn get_small(client: &HttpClient, url: &Uri, limit: usize) -> Result<Option<Body>> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header(ACCEPT_ENCODING, "identity")
        .body(Empty::<Bytes>::new())
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;

    let resp = client.request(req).await?;
    let status = resp.status();
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(Error::Status(status.as_u16()));
    }

    let content_type = header_string(resp.headers(), hyper::header::CONTENT_TYPE);
    let mut body = resp.into_body();
    let mut buf = bytes::BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > limit {
                return Ok(None);
            }
            buf.extend_from_slice(&data);
        }
    }
    Ok(Some(Body {
        content_type,
        bytes: buf.freeze(),
    }))
}
