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
/// Inactividad máxima en peticiones pequeñas (páginas, `robots.txt`).
const SMALL_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Espera `fut` como mucho `idle`; si el servidor se calla, es un error reintentable y no un bloqueo.
async fn within<T>(
    idle: std::time::Duration,
    what: &str,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::time::timeout(idle, fut)
        .await
        .map_err(|_| Error::Timeout(format!("{} s ({what})", idle.as_secs_f32())))
}
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

/// ¿Pueden viajar las cabeceras de sesión (cookies…) de la petición original a `target`?
///
/// Solo al mismo sitio (host y puerto) y sin degradar el cifrado: de `https` a `http` nunca, aunque
/// el host sea el mismo, porque una cookie obtenida para HTTPS viajaría en claro. Pasar de `http` a
/// `https` sí está permitido (sube la protección).
pub fn may_forward_session(origin: &Uri, target: &Uri) -> bool {
    let host = |u: &Uri| u.authority().map(|a| a.as_str().to_ascii_lowercase());
    if host(origin).is_none() || host(origin) != host(target) {
        return false;
    }
    match (origin.scheme_str(), target.scheme_str()) {
        (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => true,
        (Some(a), Some(b)) => a.eq_ignore_ascii_case("http") && b.eq_ignore_ascii_case("https"),
        _ => false,
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
pub async fn probe(
    client: &HttpClient,
    raw: &str,
    extra: &HeaderMap,
    idle: std::time::Duration,
) -> Result<Remote> {
    let mut uri = parse_url(raw)?;
    let origin = uri.clone();

    for _ in 0..=MAX_REDIRECTS {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(&uri)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, "bytes=0-0")
            .body(Empty::<Bytes>::new())
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if may_forward_session(&origin, &uri) {
            apply_extra(&mut req, extra);
        }

        let resp = within(idle, "sondeo", client.request(req)).await??;
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
/// Qué pedir y con qué comprobaciones.
pub struct ChunkRequest<'a> {
    pub url: &'a Uri,
    pub chunk: Chunk,
    /// Validador fuerte para `If-Range`.
    pub if_range: Option<&'a str>,
    pub extra: &'a HeaderMap,
    /// Tamaño total del recurso: la respuesta debe declararlo igual.
    pub total: u64,
    /// Inactividad máxima en cada espera.
    pub idle: std::time::Duration,
}

/// Dónde se anotan los bytes recibidos y cómo se frena la lectura.
pub struct Sinks<'a> {
    pub received: &'a [&'a std::sync::atomic::AtomicU64],
    pub limiter: Option<&'a crate::limits::RateLimiter>,
}

pub async fn fetch_chunk(
    client: &HttpClient,
    request: &ChunkRequest<'_>,
    sinks: &Sinks<'_>,
) -> Result<Bytes> {
    let ChunkRequest {
        url,
        chunk,
        if_range: etag,
        extra,
        total,
        idle,
    } = *request;
    let Sinks { received, limiter } = *sinks;
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

    let resp = within(idle, "fragmento", client.request(req)).await??;
    let status = resp.status();
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(Error::RangeIgnored(status.as_u16()));
    }
    // Un 206 con la longitud correcta pero de otra posición mezclaría el archivo sin que nada falle.
    validate_content_range(resp.headers(), chunk, total)?;

    let expected = chunk.len();
    let mut body = resp.into_body();
    let mut buf = bytes::BytesMut::with_capacity(expected as usize);
    while let Some(frame) = within(idle, "datos", body.frame()).await? {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            if let Some(limiter) = limiter {
                limiter.throttle(data.len() as u64).await;
            }
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
    idle: std::time::Duration,
) -> Result<Response<Incoming>> {
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header(ACCEPT_ENCODING, "identity")
        .body(Empty::<Bytes>::new())
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;
    apply_extra(&mut req, extra);

    let resp = within(idle, "respuesta", client.request(req)).await??;
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

/// `Content-Range: bytes inicio-fin/total` de un 206. El total puede ser `*` (desconocido).
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.trim().split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?, total))
}

/// Exige que la respuesta cubra exactamente el fragmento pedido y que el recurso siga teniendo el
/// mismo tamaño total.
fn validate_content_range(headers: &HeaderMap, chunk: Chunk, total: u64) -> Result<()> {
    let value = headers
        .get(hyper::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::BadContentRange("falta la cabecera en un 206".into()))?;
    let bad = |why: &str| Error::BadContentRange(format!("{value} ({why})"));
    let (start, end, size) = parse_content_range(value).ok_or_else(|| bad("formato no válido"))?;
    if (start, end) != (chunk.start, chunk.end) {
        return Err(bad(&format!(
            "se pidió {}-{} y el servidor entregó otra posición",
            chunk.start, chunk.end
        )));
    }
    if size.is_some_and(|size| size != total) {
        return Err(bad(&format!("el recurso medía {total} bytes")));
    }
    Ok(())
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
    fn la_sesion_no_viaja_a_otro_sitio_ni_degrada_el_cifrado() {
        let u = |s: &str| s.parse::<Uri>().unwrap();
        let https = u("https://sitio.test/a");
        assert!(may_forward_session(&https, &u("https://sitio.test/b")));
        assert!(
            may_forward_session(&https, &u("https://SITIO.test/b")),
            "el host no distingue mayúsculas"
        );
        assert!(
            !may_forward_session(&https, &u("http://sitio.test/b")),
            "https -> http nunca"
        );
        assert!(!may_forward_session(&https, &u("https://otro.test/b")));
        assert!(
            !may_forward_session(&https, &u("https://sitio.test:8443/b")),
            "otro puerto, otro sitio"
        );
        assert!(!may_forward_session(
            &https,
            &u("https://sitio.test.otro.test/b")
        ));
        // Subir de http a https es seguro.
        assert!(may_forward_session(&u("http://sitio.test/a"), &https));
        assert!(may_forward_session(
            &u("http://sitio.test/a"),
            &u("http://sitio.test/z")
        ));
        assert!(!may_forward_session(
            &u("http://sitio.test/a"),
            &u("ftp://sitio.test/z")
        ));
    }

    #[test]
    fn content_range_de_un_fragmento() {
        assert_eq!(
            parse_content_range("bytes 5-9/100"),
            Some((5, 9, Some(100)))
        );
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
        assert_eq!(
            parse_content_range(" bytes  0-0 / 7 "),
            Some((0, 0, Some(7)))
        );
        for bad in [
            "",
            "bytes */100",
            "items 0-1/2",
            "bytes a-b/c",
            "bytes 5/100",
            "5-9/100",
        ] {
            assert_eq!(parse_content_range(bad), None, "{bad}");
        }
    }

    #[test]
    fn solo_se_acepta_el_rango_pedido_y_el_mismo_tamano() {
        let chunk = Chunk {
            index: 1,
            start: 100,
            end: 199,
        };
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(hyper::header::CONTENT_RANGE, v.parse().unwrap());
            h
        };
        assert!(validate_content_range(&with("bytes 100-199/1000"), chunk, 1000).is_ok());
        assert!(validate_content_range(&with("bytes 100-199/*"), chunk, 1000).is_ok());
        for bad in [
            "bytes 99-198/1000",
            "bytes 100-198/1000",
            "bytes 100-199/1001",
            "basura",
        ] {
            assert!(
                validate_content_range(&with(bad), chunk, 1000).is_err(),
                "{bad}"
            );
        }
        assert!(validate_content_range(&HeaderMap::new(), chunk, 1000).is_err());
    }

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

    let resp = within(SMALL_IDLE, "página", client.request(req)).await??;
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
    while let Some(frame) = within(SMALL_IDLE, "página", body.frame()).await? {
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
