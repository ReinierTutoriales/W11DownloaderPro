# W11DownloaderPro

Motor de descargas acelerado para Windows 11. Fase 1: HTTP/HTTPS con segmentación por rangos, reanudación y verificación SHA-256.

## Uso

Descarga directa:

```
w11dl <URL> [-o salida] [-c conexiones] [--max-chunk bytes] [--sha256 hex]
```

Rastreo de una página para capturar archivos (respeta `robots.txt`, solo mismo host):

```
w11dl <URL_INICIO> --crawl [--ext zip,pdf,mp4] [--depth 2] [--max-pages 200] [--delay-ms 500] [--dry-run] [--out-dir carpeta]
```

Proxy: `--proxy http://host:puerto` para uno explícito, `--no-proxy` para conexión directa. Sin ninguno, se usa el proxy del sistema (en Windows, la configuración de Internet; en otros sistemas, `HTTP_PROXY`, `HTTPS_PROXY` y `NO_PROXY`).

- Reparte el archivo en fragmentos que las conexiones reclaman dinámicamente.
- Si el servidor no atiende rangos, descarga en un solo flujo (sin reanudación).
- Si se interrumpe, al repetir el comando retoma desde el estado guardado en `<salida>.part.state`.
- El archivo final solo aparece al completarse y, si se indicó `--sha256`, al verificar el hash.

## Desarrollo

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-gnu --all-targets
```

Políticas del proyecto: ver `POLITICAS.md`.

## Host de mensajería nativa (fase 3)

`w11dl-host.exe` recibe las descargas que captura la extensión del navegador.

```
w11dl-host.exe register --extension-id <id-de-14-letras-a-p>   # escribe el manifiesto y lo registra en Chrome y Edge (HKCU)
w11dl-host.exe unregister                                     # quita el registro
```

- Protocolo: mensajes JSON con prefijo de longitud (4 bytes little-endian), según Native Messaging.
- Orden de entrega: el host responde `accepted` antes de descargar. La extensión solo debe cancelar su descarga del navegador al recibir ese acuse.
- Solo se aceptan URL `http`/`https` y las cabeceras `Cookie`, `Referer`, `User-Agent` y `Accept-Language`.
- Las cabeceras de sesión solo se envían al host original; una redirección a otro dominio no las recibe.
- Las descargas van a `%USERPROFILE%\Downloads` por defecto, con `--out-dir` para cambiarlo. Nunca se sobrescribe un archivo existente: se añade ` (2)`, ` (3)`…
