//! Proxy HTTP y HTTPS. Resolución de la configuración (flag, variables de entorno o
//! configuración del sistema en Windows) y túnel CONNECT para destinos HTTPS.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::Uri;
use hyper::header::HeaderValue;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_service::Service;

const MAX_CONNECT_RESPONSE: usize = 8 * 1024;

/// Cómo llegar a los servidores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProxyMode {
    /// Proxy del sistema: en Windows, el registro; en otros, `HTTP_PROXY`, `HTTPS_PROXY` y `NO_PROXY`.
    #[default]
    System,
    /// Conexión directa, sin proxy.
    Direct,
    /// Proxy explícito, p. ej. `http://proxy:8080`.
    Url(String),
}

fn matcher(mode: &ProxyMode) -> Matcher {
    match mode {
        ProxyMode::System => Matcher::from_system(),
        ProxyMode::Direct => Matcher::builder().build(),
        ProxyMode::Url(url) => Matcher::builder().all(url.as_str()).build(),
    }
}

/// Conector que enruta por proxy cuando la configuración lo indica.
#[derive(Clone)]
pub struct ProxyConnector {
    http: HttpConnector,
    matcher: Arc<Matcher>,
}

impl ProxyConnector {
    pub fn new(mode: &ProxyMode) -> Self {
        let mut http = HttpConnector::new();
        // HttpsConnector delega aquí tanto http como https, así que no debe rechazar https.
        http.enforce_http(false);
        Self {
            http,
            matcher: Arc::new(matcher(mode)),
        }
    }
}

/// Conexión lista para hyper. Marca si pasa por proxy y encapsula el socket.
pub struct Proxied {
    io: TokioIo<TcpStream>,
    /// `true` cuando las peticiones HTTP van en forma absoluta hacia el proxy.
    proxied: bool,
}

impl Connection for Proxied {
    fn connected(&self) -> Connected {
        Connected::new().proxy(self.proxied)
    }
}

impl Read for Proxied {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl Write for Proxied {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

impl Service<Uri> for ProxyConnector {
    type Response = Proxied;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Proxied>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.http.poll_ready(cx).map_err(io::Error::other)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let mut http = self.http.clone();
        let intercept = self.matcher.intercept(&dst);

        Box::pin(async move {
            let Some(intercept) = intercept else {
                let io = http.call(dst).await.map_err(io::Error::other)?;
                return Ok(Proxied { io, proxied: false });
            };

            let io = http
                .call(intercept.uri().clone())
                .await
                .map_err(io::Error::other)?;
            let mut stream = io.into_inner();

            if dst.scheme_str() == Some("https") {
                // El túnel es transparente: hyper-rustls negocia TLS con el destino real.
                tunnel(&mut stream, &dst, intercept.basic_auth()).await?;
                Ok(Proxied {
                    io: TokioIo::new(stream),
                    proxied: false,
                })
            } else {
                // HTTP plano: el cliente envía la URL absoluta al proxy.
                Ok(Proxied {
                    io: TokioIo::new(stream),
                    proxied: true,
                })
            }
        })
    }
}

/// Abre un túnel CONNECT hacia el destino. Falla si el proxy no responde 200.
pub async fn tunnel(
    stream: &mut TcpStream,
    dst: &Uri,
    auth: Option<&HeaderValue>,
) -> io::Result<()> {
    let host = dst
        .host()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destino sin host"))?;
    let port = dst.port_u16().unwrap_or(443);
    let target = format!("{host}:{port}");

    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(value) = auth {
        let value = value.to_str().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "credenciales de proxy no válidas",
            )
        })?;
        request.push_str(&format!("Proxy-Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;

    // Lee byte a byte hasta el fin de cabeceras: no debe consumir el primer byte de TLS.
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_CONNECT_RESPONSE {
            return Err(io::Error::other("respuesta de CONNECT demasiado grande"));
        }
        if stream.read(&mut byte).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }

    let status_line = head.split(|b| *b == b'\n').next().unwrap_or_default();
    let code = String::from_utf8_lossy(status_line)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    if code == "200" {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "el proxy rechazó el túnel con {code}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tunel_envia_destino_y_credenciales_y_acepta_200() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                sock.read_exact(&mut byte).await.unwrap();
                req.push(byte[0]);
            }
            sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(req).unwrap()
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let dst: Uri = "https://destino.test/archivo.zip".parse().unwrap();
        let auth = HeaderValue::from_static("Basic dXN1YXJpbzpjbGF2ZQ==");
        tunnel(&mut stream, &dst, Some(&auth)).await.unwrap();

        let request = proxy.await.unwrap();
        assert!(request.starts_with("CONNECT destino.test:443 HTTP/1.1\r\n"));
        assert!(request.contains("Proxy-Authorization: Basic dXN1YXJpbzpjbGF2ZQ==\r\n"));
    }

    #[tokio::test]
    async fn tunel_rechazado_por_el_proxy_es_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let dst: Uri = "https://destino.test/".parse().unwrap();
        let err = tunnel(&mut stream, &dst, None).await.unwrap_err();
        assert!(err.to_string().contains("407"), "{err}");
    }
}
