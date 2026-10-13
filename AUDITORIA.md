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
**Herramientas incluidas en el paquete de Windows** (carpeta `tools`), fijadas en `packaging/third-party.json` y verificadas por SHA-256 al empaquetar (`packaging/bundle.py` aborta si un hash no coincide):
- `yt-dlp.exe` 2026.08.19 (ejecutable oficial; el hash coincide con el `SHA2-256SUMS` de su release). Licencia Unlicense.
- `ffmpeg.exe` y `ffprobe.exe` n9.0.2-27-g7e93014c5b (BtbN/FFmpeg-Builds, variante LGPL sin componentes GPL; hash comprobado descargando el archivo). Licencia LGPL 2.1+; requiere conservar `FFMPEG-LICENSE.txt` y ofrecer el código fuente (ver `NOTICES.txt`).
- **El pin de ffmpeg apunta a una compilación fechada de un tercero**: si BtbN la retira, el empaquetado falla (a propósito) y hay que actualizar el pin. Sin ese paquete incluido, ffmpeg del sistema sigue siendo un binario sin verificar.
- Los ejecutables propios y los de terceros **no están firmados**.

## 4. Funciones del motor y del host

### 4.1 Descarga segmentada (`engine.rs`, `plan.rs`)
- Fragmentos de tamaño efectivo según archivo y conexiones; cola afilada al final (fragmentos más pequeños).
- Controlador adaptativo de conexiones: arranque por tamaño (4 si ≥ 8 MiB), crecimiento ×1,10, retroceso al mejor punto tras 2 caídas, medición continua de bytes recibidos.
- **Copia de fragmentos lentos (hedging):** con la cola vacía, una conexión libre compite contra un fragmento muy lento; gana el primero. No se usa con rechazos del servidor ni con límite de velocidad.
- **Rechazos 403/429/503:** el fragmento vuelve a la cola, se fija un tope de conexiones que se relaja +1 cada 60 s.
- **Límite de velocidad** (`limits.rs`): cubo de fichas compartido, con reparto global si se comparte el mismo objeto.
- Un solo guardado de estado a la vez (candado); antes dos checkpoints simultáneos podían fallar con `NotFound`.

### 4.2 Reanudación (`plan.rs: State`, `Validators`)
- Estado en `.part.state`. Se reanuda si URL y tamaño coinciden, ningún validador presente en ambos lados difiere y hay **verificación del contenido**: un ETag fuerte o un `Last-Modified` presentes en ambos lados y coincidentes (un ETag débil no basta).
- **Sin esa verificación no se reanuda** (empieza de cero), salvo que se indique `--sha256`: el hash final detecta cualquier mezcla y el archivo no se publica si no coincide. Cumple `POLITICAS.md` §7.3.
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
| Rust (`cargo test`, con `W11DL_PYTHON` y ffmpeg) | 173 pasan, 0 fallan |
| Extensión (`npm test`) | 24 pasan, 1 omitida (vídeo, requiere `W11DL_PYTHON`) |
| Botón en Chromium con Playwright (`test/video-button.dom.js`) | 6 comprobaciones OK |
| `cargo clippy --all-targets -- -D warnings` | sin avisos |
| `cargo check --target x86_64-pc-windows-gnu --all-targets` | compila |
| GitHub Actions en `windows-latest` (compilación MSVC nativa, paquete, `cargo test` completo con yt-dlp.exe y ffmpeg.exe incluidos, `npm test`) | verde en el commit `ec575c8`; primera ejecución real en Windows de todo el motor, la cola, el host, HLS con yt-dlp.exe y la unión con ffmpeg |

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

1. **~~Reanudación por tamaño sin validadores~~ (resuelto, opción C):** por defecto no se reanuda sin verificación del contenido; con `--sha256` sí, y un archivo cambiado con el mismo tamaño se detecta por el hash (no se publica). Cubierto por pruebas de integración y validado por mutación en las dos direcciones. Efecto práctico: contra un servidor sin ETag fuerte ni `Last-Modified`, una descarga interrumpida empieza de cero; el host de la extensión no usa `--sha256`, así que se comporta así siempre.
2. **~~Descargas por POST se rompían~~ (corregido):** `shouldIntercept` solo intercepta peticiones GET; POST y cualquier otro método (o un método desconocido) los baja el propio navegador. Cubierto por prueba unitaria y validado por mutación. No probado en Firefox real: depende de que `details.method` llegue en `onHeadersReceived`, que la documentación de la API de WebExtensions da como campo estándar.
3. **Solo se capturan navegaciones principales** (`main_frame`): descargas por `fetch`, `XHR`, `blob:` o iframes no se capturan.
4. **Autenticación HTTP o por certificado no viaja**; solo cookies, `Referer` y `User-Agent`.
5. **"En curso" engañoso:** una descarga que espera turno (más de 4) ya figura en curso.
6. **`panic = "abort"` en release:** un pánico en cualquier tarea cierra todo el host y las descargas en curso.
7. **Licencia sin decidir:** `license = "UNLICENSED"` en `Cargo.toml`.
8. **Vídeo delegado en yt-dlp:** no recibe las cookies del navegador; el límite de velocidad no cambia en caliente; el informe lo marca como `Single` aunque use 8 fragmentos.
9. **Descargas en espera** no llevan cookies (el host no las guarda en disco).
10. **Resumir un vídeo con pistas separadas tras reiniciar el host** no es posible (los enlaces viven en memoria); las delegadas en yt-dlp sí.
11. **Procesos externos que se ejecutan:** `yt-dlp` (incluido, o un Python con `yt_dlp`), `ffmpeg`, `schtasks`, `shutdown`, `rundll32`. La ruta de ffmpeg sale de `W11DL_FFMPEG`, de `tools` junto al ejecutable, de la carpeta de datos o del PATH; la de yt-dlp, de `W11DL_PYTHON`, de `tools` o del entorno gestionado. Conviene revisar si en Windows un nombre suelto podría resolverse en una ubicación no deseada (se resuelve manualmente por el PATH, pero la comprobación `-version` se lanza con el nombre suelto).
12. **No se ha ejecutado `cargo audit` ni `cargo deny`** (no instalados en este entorno).

Procedimiento: varios cambios (p. ej. el selector de vídeo, la serialización de checkpoints, la carpeta de intermedios) se implementaron tras una aprobación general del dueño ("continúa", "ok") y no con una propuesta individual previa, que es lo que pide `POLITICAS.md` §5.

## 7 bis. Revisión cruzada 1 (hallazgos de una segunda revisión, contrastados uno a uno con el código)

Cada hallazgo se comprobó leyendo el código y, cuando fue posible, **reproduciéndolo con una prueba que fallaba antes del arreglo**.

| # | Hallazgo | Comprobación | Corrección | Prueba |
|---|---|---|---|---|
| 1 | Dos descargas con el mismo nombre comparten archivo, parcial y estado | **Reproducido**: tres peticiones en espera recibían la misma ruta; un parcial ajeno no ocupaba el nombre | Destino adjudicado de forma atómica (disco, cola, reservas, extensiones posibles de un vídeo) | `tests/destinos.rs` |
| 1b | Dos escritores sobre la misma ruta | **Reproducido** (la segunda fallaba con `NotFound` y pisaba el parcial) | Bloqueo exclusivo del sistema sobre el parcial; la segunda recibe `Busy` | `dos_descargas_a_la_misma_ruta_no_se_pisan` |
| 2 | `Content-Range` del fragmento sin validar | **Reproducido** en 4 casos (otra posición, sin cabecera, otro inicio, otro tamaño total): se publicaba un archivo corrupto | Se exige posición exacta y mismo tamaño total | `tests/rangos.rs`, unitarias en `net.rs` |
| 3 | Longitud del parcial no comprobada al reanudar | **Reproducido** (truncado y con bytes de más) | Solo se reanuda si el parcial mide exactamente el tamaño final | `parcial_truncado…`, `parcial_mas_largo…` |
| 4 | Cookies reenviadas al degradar https a http en el mismo host | Confirmado por lectura (solo se comparaba el host); **no reproducido de extremo a extremo** (haría falta un servidor HTTPS) | `may_forward_session`: mismo sitio, sin degradar el cifrado | Unitarias en `net.rs` y `engine.rs` |
| 5 | Guardado de la cola sin serializar | **Reproducido**: 222 de 480 guardados fallaban | Candado que cubre captura, escritura y renombrado; el primer fallo se comunica una vez | `guardados_simultaneos…`, `un_guardado_que_falla_avisa…` |
| 6 | Sin tiempos de inactividad; pausa/cancelación esperaban a la red | **Reproducido** en 8 casos (servidor callado en sondeo, fragmentos, datos y sin rangos) | Inactividad máxima de 30 s por espera (configurable) y el control corta cualquier espera | `tests/bloqueos.rs` |
| 7 | `Last-Modified` tratado siempre como fuerte | Confirmado por lectura | Solo es fuerte si es ≥ 1 s anterior a `Date` (RFC 9110 §8.8.2.2); si no, ni `If-Range` ni verificación | Unitarias y `last_modified_en_el_futuro…` |

Hallazgos propios de esta ronda: en Unix, el yt-dlp autónomo dejaba un proceso hijo vivo al cancelar (prueba intermitente), ahora se mata el grupo de procesos entero; clippy más estricto con Rust 1.89 (mínimo actualizado).
Se añadió a la CI un trabajo `calidad`: `cargo fmt --check`, `cargo clippy -D warnings` y `cargo audit` (RustSec). **`cargo audit` ejecutado localmente: 0 vulnerabilidades en 194 dependencias** (base de avisos de hoy).
Siguen sin verificarse: los efectos del bloqueo de archivos de Windows (mandatorio) en la CI de Windows de esta rama, y todo lo de la sección 8.

## 7 ter. Revisión cruzada 2 (segunda ronda, contrastada igual que la primera)

| # | Hallazgo | Comprobación | Corrección | Prueba |
|---|---|---|---|---|
| A | El bloqueo del parcial se soltaba antes de verificar y publicar | **Reproducido**: una segunda descarga entraba mientras la primera calculaba el hash (96 MiB) y descargaba todo de nuevo | El bloqueo se toma al empezar, **antes de leer el estado o la longitud**, y se mantiene hasta publicar y limpiar; el hash se lee por el mismo manejador (en Windows el bloqueo es obligatorio) | `tests/ciclo.rs` |
| B | Dos procesos con la misma cola (host del navegador y `run-queue`) | **Reproducido con procesos reales**: no había exclusión alguna | Un propietario por carpeta (`.w11dl-queue.lock`): `run-queue` se niega con código 2 y sin acción final; un segundo host funciona pero no guarda la cola y avisa una vez; la tarea de Windows reintenta cada 10 min (sin verificar en Windows) | `tests/procesos.rs` |
| C | `throttle` fuera del `select!` en la descarga sin rangos | **Reproducido**: cancelar con 1 KiB/s no respondía | La espera del limitador compite con el control | `sin_rangos_cancelar_corta…` |
| D | Cancelar durante el sondeo no limpiaba el parcial previo | **Reproducido** | Con el destino bloqueado, cancelar borra los restos propios; pausar los conserva; quien no es dueño (`Busy`) no borra nada | `cancelar_durante_el_sondeo…`, `cancelar_una_descarga_rechazada…` |
| E | Fechas HTTP inválidas aceptadas; política de confianza en `Last-Modified` | Confirmado: se aceptaban días inexistentes de un mes y horas sin validar el ancho | Gramática estricta (ancho fijo, solo dígitos, bisiestos, día de la semana coherente) y margen de 60 s entre `Date` y `Last-Modified`: origen y CDN pueden tener relojes distintos, así que ante la duda no se fía | `fechas_http_invalidas…`, `last_modified_es_fuerte_solo_con_margen…` |

Coste asumido de E: un archivo modificado hace menos de un minuto no se reanuda sin SHA-256 (se pierde progreso, no integridad).
Ronda 3 (revisión de 66f1ecb):

| Hallazgo | Estado | Corrección | Prueba |
|---|---|---|---|
| La prueba del ciclo calculaba el SHA-256 esperado dentro de `make(...)`, tras la transferencia, y el criterio de 500 ms permitía pasar sin exigir `Busy` | Confirmado y retirado el criterio | Barrera determinista solo con la feature `test-hooks` (`engine::Gate`): la primera se detiene con el destino bloqueado justo antes de verificar; la segunda debe recibir `Busy`; luego se libera y se comprueban contenido y limpieza. Hash y opciones se preparan antes de arrancar. **Comprobación de mutación:** con `lock_exclusive` anulado la prueba falla (la segunda descargaba de nuevo); restaurado, pasa | `tests/ciclo.rs` |
| Un segundo host adoptaba la cola ajena (`running` → `interrupted`) y podía cancelar y borrar parciales sin bloqueo; aceptaba esperas que no se guardaban | Corregido | Sin propiedad de la cola, la instancia no carga registros ajenos (empieza vacía), rechaza `queued` con un error explícito, y el borrado de parciales de una cancelación toma antes el bloqueo del destino (`engine::discard_partials`, `Busy` si está en uso). **Mutación:** adoptando la cola ajena la prueba falla | `tests/procesos.rs` (procesos reales: cancelar/pausar/reanudar desde el segundo host devuelven error, el parcial sigue, la cola no se reescribe y la descarga del primero termina intacta) |
| `refusal`: una medición y factor fijo | Separado | Pruebas funcionales (completa, contenido, ≤ 8 rechazos) distintas del control de rendimiento (mejor de 3 frente al mejor de 3 de un solo flujo, factor 2 + 500 ms) | `tests/refusal.rs` |

Ronda 4 (revisión de acbdb4d, vídeo, cancelación y reanudación). Cada punto se comprobó con una prueba que falla si se retira la corrección (mutación):

| Hallazgo | Corrección | Prueba |
|---|---|---|
| Reanudar un vídeo con pistas tras reiniciar bajaba solo la imagen y daba el trabajo por terminado (`Entry.merge` solo en memoria) | `Record` persiste `merged` y `page`. Al reanudar (host o `run-queue`) sin pareja de pistas se vuelve a resolver la página; si ya no ofrece imagen+sonido o falta ffmpeg, el trabajo pasa a `Failed` con un mensaje claro y no se pide ninguna pista. Si el host no encuentra ffmpeg para un vídeo con sonido aparte, lo rechaza en lugar de bajar solo la imagen | `video_ciclo::reanudar_un_video_con_pistas_tras_reiniciar_nunca_baja_solo_la_imagen` |
| Pausar o cancelar durante la unión no se atendía (`output().await`) | ffmpeg se ejecuta con `spawn` y se compite con `Control::interrupted()`: se termina y se espera el proceso; pausar conserva las pistas, cancelar borra todo, y el control se vuelve a comprobar antes de publicar | `cancelar_durante_la_union…`, `pausar_durante_la_union…` (ffmpeg falso que se queda colgado) |
| El destino del vídeo no se reservaba durante todo el trabajo (`.merging` compartido, `-y`) | `engine::DestinationLock` toma el mismo bloqueo que una descarga directa sobre ese destino; `download_merged` y el camino de yt-dlp lo mantienen de principio a fin | `un_segundo_trabajo_al_mismo_destino…` (otra unión y una descarga directa reciben `Busy`) |
| Cancelar una unión pausada llamaba a `discard_partials` sobre el destino final y dejaba `.video`/`.audio` | La cancelación se decide por tipo de trabajo (`merge::discard`, directo, yt-dlp), bajo exclusión del destino | `cancelar_una_union_pausada…` |
| `discard_partials` ignoraba errores de borrado | Tolera «no existe» y comunica el resto; la cancelación del host no se confirma si dejó restos. Lo mismo en las cancelaciones de unión y en la del motor durante el sondeo | `cancelar_comunica_los_fallos_de_borrado…`, `el_borrado_de_parciales…` |
| Cambiar la velocidad en caliente no despertaba esperas ya iniciadas (64 KiB a 1 KiB/s ≈ 64 s) | El limitador se rehízo como deuda en tiempo: `set_rate` reescala la deuda pendiente y avisa por un canal `watch`; las esperas la recalculan (quitar el límite las corta en el acto) | `limits::quitar_el_limite_o_subirlo…`, `la_deuda_pendiente_sobrevive…` |
| `claim_path` comparaba rutas literalmente (`Video.mp4` ≠ `video.mp4` en Windows) | La reserva y la comparación usan una identidad sin distinguir mayúsculas en Windows | unitaria multiplataforma de la identidad; la de `claim_path` es `cfg(windows)` (se ejecuta en la CI de Windows, no se ha ejecutado aquí) |

Límites conocidos de esta ronda: el ffmpeg falso de las pruebas en Windows (`.cmd`) no se ha ejecutado aún en la CI; la comparación sin mayúsculas no cubre nombres cortos 8.3 ni normalización Unicode. (La afirmación de que el motor descartaba las pistas con validadores distintos solo era cierta para las parciales; las terminadas se corrigen en la ronda 5.)

Ronda 5 (revisión de 4b45d83):

| Hallazgo | Corrección | Prueba |
|---|---|---|
| Las pistas completas se reutilizaban solo por existir (`fetch_track`), pudiendo unir pistas de contenidos o selecciones distintos | Cada pista completa lleva un manifiesto `.done` (tamaño + `ETag` fuerte o `Last-Modified` fuerte, `engine::identify`), escrito solo si el recurso era el mismo antes y después de bajarlo. Se reutiliza únicamente si el manifiesto coincide con la identidad actual y con la longitud en disco; sin manifiesto, sin validador fuerte o con cualquier diferencia, se borra y se vuelve a bajar. Un enlace distinto no decide nada por sí solo | `una_pista_completa_sin_manifiesto_o_con_otro_tamano…`, `tests/merge.rs` (la pista cambiada se baja de nuevo y la intacta se reutiliza). **Mutación:** reutilizando por existencia la prueba falla |
| Soltar el bloqueo y después borrar el marcador abría una carrera (`discard_partials`, `DestinationLock::drop`, parcial renombrado) | Exclusión por archivo estable y vacío `.w11dl-locks/<destino>.lock` junto al destino, que nunca se renombra ni se borra. Se mantiene durante estado, transferencia, verificación, publicación y toda la limpieza; el parcial ya no es el objeto bloqueado. Deja un archivo vacío por destino en esa carpeta | `el_archivo_de_exclusion_es_estable…` (tensión: limpiezas repetidas contra descargas nuevas). **Mutación:** soltando el bloqueo antes de borrar el parcial falla, aunque de forma intermitente (prueba de carrera, no determinista); la propiedad se sostiene además por construcción y por `ciclo.rs` |
| Una pausa recibida al terminar ffmpeg podía publicar | Se comprueba cualquier interrupción antes de publicar: pausar retira el temporal, conserva las pistas y devuelve `Paused`; cancelar borra todo. Barrera `test-hooks` entre el fin de ffmpeg y la publicación | `una_pausa_que_llega_al_terminar_la_union_no_publica`. **Mutación:** comprobando solo la cancelación falla |
| `abandon_single` confirmaba una cancelación con el parcial sin borrar | Devuelve `Result<Step>` y propaga el error de borrado (con la exclusión tomada) | cubierto por la ruta de limpieza; sin prueba específica de fallo de borrado en el modo sin rangos (pendiente) |

Corrección de una afirmación anterior: la prueba de cicloCorrección de una afirmación anterior: la prueba de ciclo no pasa por el adjudicador de nombres del host (ambas descargas usan el mismo destino), y `procesos.rs` no cubría dos descargas compitiendo durante el hash y la publicación; ahora lo cubren `ciclo.rs` (mismo proceso, determinista) y el bloqueo exclusivo del sistema operativo.

Pendiente de verificar: el comportamiento de estos bloqueos y de `RestartOnFailure` en la CI y en un Windows 11 de escritorio.

## 8. No verificado

- **Windows 11 de escritorio:** la CI ejecuta en un runner Windows Server (`windows-latest`) el motor, la cola, el host por canal en memoria, HLS con yt-dlp.exe y la unión con ffmpeg.exe. Sigue sin verificarse allí el registro del host (HKCU) para Firefox, la comunicación stdio real con el navegador, `schtasks` con el XML en UTF-16, el apagado y la suspensión.
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

1. Sección 7: los puntos 1 y 2 están resueltos; revisar que el código y las pruebas lo confirmen (`plan.rs: State::matches`, `engine.rs`, `tests/engine.rs`).
2. `extension/manifest.json`: permisos (`<all_urls>`, `webRequestBlocking`, `cookies`, script de contenido en todas las páginas http/https).
3. `src/native.rs: prepare`: validación de URL, cabeceras y nombre de archivo.
4. `src/ytdlp.rs`, `src/merge.rs`, `src/video.rs: find_ffmpeg`: ejecución de procesos y rutas.
5. `src/engine.rs`: publicación atómica, checkpoints, cancelación y borrado de parciales.
6. `src/schedule.rs` y `w11dl-host schedule`: XML de la tarea y acción de energía.
7. Comprobar la regla de autoría (`POLITICAS.md` §4): `git log --format='%an %ae' | sort -u` debe mostrar un único autor, y `git log --format=%B | grep -i "co-authored"` no debe devolver nada. Buscar además en el árbol los nombres de asistentes o herramientas que no quieras ver publicados.
