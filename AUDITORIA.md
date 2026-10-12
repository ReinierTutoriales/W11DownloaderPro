# Estado del proyecto para auditoría

Rama: `politicas-y-motor` (37 commits, todos con la identidad del dueño). `main` no incluye nada de lo posterior al PR 2.
Fecha del inventario: 2026-10-12. Todo lo que figura aquí se contrastó con el código o con una ejecución; lo que no se pudo comprobar está en la sección 8.

## 1. Qué es

Acelerador de descargas para Windows 11 en tres piezas:

| Pieza | Qué hace | Dónde |
|---|---|---|
| Motor (`w11dl`) | Descarga segmentada por rangos HTTP, reanudable, con verificación SHA-256 | `src/engine.rs`, `src/plan.rs`, `src/net.rs` |
| Host de mensajería nativa (`w11dl-host`) | Recibe las descargas del navegador, mantiene la cola, ejecuta vídeo y programación | `src/native.rs`, `src/queue.rs`, `src/bin/w11dl-host.rs` |
| Extensión de Firefox (MV3) | Captura descargas, botón sobre vídeos, popup con cola y límite | `extension/` |

## 2. Flujo de una descarga

1. **Captura.** `webRequest.onHeadersReceived` (bloqueante) mira las navegaciones principales. Si la respuesta es `Content-Disposition: attachment` o un tipo binario de la lista (`extension/lib/core.js`), cancela la navegación y entrega la URL al host con las cookies, el `Referer` y el `User-Agent`.
2. **Respaldo.** Si el host no acusa recibo en 10 s o falla, la extensión pide la misma URL al navegador (`downloads.download`) y la marca 30 s para no interceptarla otra vez.
3. **Host.** Valida URL (solo http/https), cabeceras (lista blanca de 4) y nombre de archivo; responde `accepted`; registra en la cola; ejecuta como máximo 4 a la vez.
4. **Motor.** Sondeo `Range: bytes=0-0`, redirecciones manuales, segmentación con controlador adaptativo, escritura en `.part`, checkpoints con `sync_data`, publicación por renombrado atómico.
5. **Vídeo.** Solo bajo petición (menú contextual o botón sobre el vídeo): `yt-dlp` resuelve la página y `src/video.rs` elige la vía (sección 4.5).

## 3. Inventario

### 3.1 Código (líneas)

Rust `src/`: 6 402 líneas en 19 archivos; las mayores son `engine.rs` (1 000), `native.rs` (844), `plan.rs` (746), `video.rs` (612).
Extensión: `background.js` 373, `lib/core.js` 275, `content/video-button.js` 191, `popup/` 186.
Pruebas: 14 archivos de prueba y 2 módulos auxiliares en `tests/` (≈2 430 líneas) y 4 archivos de prueba de la extensión.

### 3.2 Dependencias (Rust, directas)

`tokio`, `hyper`, `hyper-util`, `hyper-rustls`, `rustls`, `http-body-util`, `bytes`, `sha2`, `clap`, `thiserror`, `serde`, `serde_json`, `tower-service`, `scraper`, `url`, `robotstxt`; en Windows `winreg`; de desarrollo `tempfile`.
Versiones fijadas en `Cargo.lock`. Python/yt-dlp: `packaging/requirements.lock` con hashes SHA-256 (se instala con `--only-binary` y `--no-cache-dir`).
**ffmpeg no está fijado ni verificado por hash**: es un binario del sistema que instala el usuario.

## 4. Funciones del motor y del host

### 4.1 Descarga segmentada (`engine.rs`, `plan.rs`)
- Fragmentos de tamaño efectivo según archivo y conexiones; cola afilada al final (fragmentos más pequeños).
- Controlador adaptativo de conexiones: arranque por tamaño (4 si ≥ 8 MiB), crecimiento ×1,10, retroceso al mejor punto tras 2 caídas, medición continua de bytes recibidos.
- **Copia de fragmentos lentos (hedging):** con la cola vacía, una conexión libre compite contra un fragmento muy lento; gana el primero. No se usa con rechazos del servidor ni con límite de velocidad.
- **Rechazos 403/429/503:** el fragmento vuelve a la cola, se fija un tope de conexiones que se relaja +1 cada 60 s.
- **Límite de velocidad** (`limits.rs`): cubo de fichas compartido, con reparto global si se comparte el mismo objeto.
- Un solo guardado de estado a la vez (candado); antes dos checkpoints simultáneos podían fallar con `NotFound`.

### 4.2 Reanudación (`plan.rs: State`, `Validators`)
- Estado en `.part.state`. Se reanuda si URL y tamaño coinciden y ningún validador presente en ambos lados (ETag normalizado, `Last-Modified`) difiere.
- **Sin validadores comparables se reanuda solo por tamaño** (ver 7.1).
- `If-Range` usa solo validadores fuertes (RFC 9110 §13.1.5).

### 4.3 Cola y host (`queue.rs`, `native.rs`)
- Estados: en curso, en espera, pausada, interrumpida, terminada, cancelada, fallida. Persistencia en `.w11dl-queue.json`; las cabeceras de sesión no se guardan.
- Mensajes: `ping`, `enqueue` (con `queued`), `enqueue_video`, `pause`, `resume`, `cancel`, `list`, `set_limit`.
- Ajustes persistentes: `.w11dl-settings.json` (solo el límite de velocidad).

### 4.4 Cola programada (`schedule.rs`, `w11dl-host run-queue|schedule|unschedule`)
- Sin proceso residente: el Programador de tareas de Windows lanza `run-queue` (tarea creada por XML con fecha ISO, despierta el equipo, recupera ejecuciones perdidas).
- Acción final (apagar con 60 s abortables, hibernar, suspender) solo si se descargó algo y no hubo fallos ni cancelaciones.

### 4.5 Vídeo (`video.rs`, `merge.rs`, `ytdlp.rs`)
Orden de decisión:
1. Archivo directo con imagen y sonido → motor propio.
2. Imagen y sonido separados por HTTPS y ffmpeg disponible → ambas pistas con el motor propio y unión con ffmpeg (`-c copy`).
3. HLS, DASH u otros contenedores sin DRM (o con más resolución que lo directo) → yt-dlp descarga con 8 fragmentos en paralelo; los intermedios van a una carpeta exclusiva.
4. DRM → se rechaza con mensaje claro. Emisiones en directo → se rechazan.
Sin ffmpeg no se delega ni se une (explica la dependencia con enlace).

### 4.6 Extensión
- Captura automática de descargas; menú contextual (enlace, vídeo de enlace, vídeo de página); popup con búsqueda de enlaces, cola (pausar, reanudar, cancelar), límite de velocidad, "dejar en espera" e interruptores.
- **Botón sobre los vídeos:** contenedor propio con Shadow DOM cerrado, sigue al vídeo bajo el puntero, solo actúa con un clic real (`isTrusted`), excluye vídeos pequeños, decorativos y con DRM (`mediaKeys`/`encrypted`).

## 5. Pruebas

| Conjunto | Resultado de la última ejecución |
|---|---|
| Rust (`cargo test`, con `W11DL_PYTHON` y ffmpeg) | 131 pasan, 0 fallan |
| Extensión (`npm test`) | 24 pasan, 1 omitida (vídeo, requiere `W11DL_PYTHON`) |
| Botón en Chromium con Playwright (`test/video-button.dom.js`) | 6 comprobaciones OK |
| `cargo clippy --all-targets -- -D warnings` | sin avisos |
| `cargo check --target x86_64-pc-windows-gnu --all-targets` | compila |

Se validaron por mutación (se rompe el código y la prueba debe fallar): `If-Range` débil, copia de fragmentos lentos, detección de DRM del botón, carrera de checkpoints (reproducida 2 de 8 y corregida 12 de 12).
Integración real: servidores locales con rangos, lentitud, rechazos, enlace revocado; host real vía Node; ffmpeg real; yt-dlp real contra un HLS local.

## 6. Mediciones (todas en bucle local con topes sintéticos; no representan una red real)

| Escenario | Antes | Después |
|---|---|---|
| 64 MB, 8 conexiones, 1 de cada 12 fragmentos a 1/10 de velocidad | 5,4 s (mediana) | 2,9 s |
| Servidor que admite 1 conexión, rechaza el resto con 503 | 2,1 s | 1,03 s (ideal ~0,75 s) |
| Límite 4 MiB/s / dos descargas con límite compartido de 6 MiB/s | — | 3,99 MiB/s / 5,93 MiB/s |
| Referencia sin fragmentos lentos (16 MB y 64 MB, tope por conexión y global) | 0,659 / 1,589 / 2,050 / 6,475 s | 0,659 / 1,593 / 2,064 / 6,472 s |

## 7. Defectos y desviaciones conocidos (revisar primero)

1. **Reanudación por tamaño sin validadores** contradice la letra de `POLITICAS.md` §7.3 ("validación del contenido"). Un servidor sin ETag ni `Last-Modified` que cambie el archivo manteniendo el tamaño produciría un archivo mezclado. Mitigación: `--sha256`. Decisión pendiente: mantener o volver a no reanudar sin validador.
2. **~~Descargas por POST se rompían~~ (corregido):** `shouldIntercept` solo intercepta peticiones GET; POST y cualquier otro método (o un método desconocido) los baja el propio navegador. Cubierto por prueba unitaria y validado por mutación. No probado en Firefox real: depende de que `details.method` llegue en `onHeadersReceived`, que la documentación de la API de WebExtensions da como campo estándar.
3. **Solo se capturan navegaciones principales** (`main_frame`): descargas por `fetch`, `XHR`, `blob:` o iframes no se capturan.
4. **Autenticación HTTP o por certificado no viaja**; solo cookies, `Referer` y `User-Agent`.
5. **"En curso" engañoso:** una descarga que espera turno (más de 4) ya figura en curso.
6. **`panic = "abort"` en release:** un pánico en cualquier tarea cierra todo el host y las descargas en curso.
7. **Licencia sin decidir:** `license = "UNLICENSED"` en `Cargo.toml`.
8. **Vídeo delegado en yt-dlp:** no recibe las cookies del navegador; el límite de velocidad no cambia en caliente; el informe lo marca como `Single` aunque use 8 fragmentos.
9. **Descargas en espera** no llevan cookies (el host no las guarda en disco).
10. **Resumir un vídeo con pistas separadas tras reiniciar el host** no es posible (los enlaces viven en memoria); las delegadas en yt-dlp sí.
11. **Procesos externos que se ejecutan:** `python` del entorno, `ffmpeg`, `schtasks`, `shutdown`, `rundll32`. La ruta de ffmpeg sale de `W11DL_FFMPEG`, de la carpeta de datos o del PATH. Conviene revisar si en Windows un nombre suelto podría resolverse en una ubicación no deseada (se resuelve manualmente por el PATH, pero la comprobación `-version` se lanza con el nombre suelto).
12. **No se ha ejecutado `cargo audit` ni `cargo deny`** (no instalados en este entorno).

Procedimiento: varios cambios (p. ej. el selector de vídeo, la serialización de checkpoints, la carpeta de intermedios) se implementaron tras una aprobación general del dueño ("continúa", "ok") y no con una propuesta individual previa, que es lo que pide `POLITICAS.md` §5.

## 8. No verificado

- **Windows 11:** nada se ha ejecutado allí. En particular el registro del host (HKCU) para Firefox, la comunicación stdio, `schtasks` con el XML en UTF-16, el apagado y la suspensión, y `ffmpeg.exe`.
- **Firefox real con las funciones recientes:** el último arnés con Firefox real ejecutó captura y respaldo (versión anterior). El botón de vídeo, el selector de límite, "dejar en espera" y la pantalla completa no se han probado en Firefox.
- **Sitios reales de vídeo:** solo HLS local. YouTube resolvió formatos pero la descarga devolvió 403 en este entorno (también con el descargador de yt-dlp).
- **Redes reales:** todas las mediciones son sintéticas.
- **Revisión legal** según el país de distribución, incluidos los términos de uso de los sitios.
- **Pendientes de diseño:** HTTP/3 (la biblioteca `h3` sigue en 0.0.x), archivo disperso en NTFS (a medir en Windows), límites de datos, categorías, multi-red.

## 9. Cómo reproducir

```
cargo build --release
W11DL_PYTHON=<python con yt-dlp> cargo test
cargo clippy --all-targets -- -D warnings
cargo check --target x86_64-pc-windows-gnu --all-targets
cd extension && npm test
NODE_PATH=<ruta a playwright> node test/video-button.dom.js   # Chromium, opcional
W11DL_TRACE=1 target/release/w11dl <url> -c 8                  # traza del controlador
```

## 10. Lista sugerida para la auditoría

1. Sección 7, punto 1 (decisión de política) y punto 2 (fallo de corrección).
2. `extension/manifest.json`: permisos (`<all_urls>`, `webRequestBlocking`, `cookies`, script de contenido en todas las páginas http/https).
3. `src/native.rs: prepare`: validación de URL, cabeceras y nombre de archivo.
4. `src/ytdlp.rs`, `src/merge.rs`, `src/video.rs: find_ffmpeg`: ejecución de procesos y rutas.
5. `src/engine.rs`: publicación atómica, checkpoints, cancelación y borrado de parciales.
6. `src/schedule.rs` y `w11dl-host schedule`: XML de la tarea y acción de energía.
7. Comprobar la regla de autoría (`POLITICAS.md` §4): `git log --format='%an %ae' | sort -u` debe mostrar un único autor, y `git log --format=%B | grep -i "co-authored"` no debe devolver nada. Buscar además en el árbol los nombres de asistentes o herramientas que no quieras ver publicados.
