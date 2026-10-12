# W11DownloaderPro — extensión de Firefox

Captura descargas de archivos del navegador y enlaces de páginas, y los envía al acelerador (`w11dl-host.exe`).

## Instalar para probar (Firefox)

1. Compila el motor y el host: `cargo build --release` en la raíz del repositorio.
2. Registra el host para Firefox (solo tu usuario, HKCU):
   ```
   target\release\w11dl-host.exe register --firefox-id w11dl@reiniertutoriales
   ```
3. Abre `about:debugging#/runtime/this-firefox` y pulsa **Cargar complemento temporal…**. Elige `extension/manifest.json`.

La extensión queda activa hasta cerrar Firefox. Para una instalación permanente hará falta firmarla en addons.mozilla.org.

## Qué hace

- **Captura automática**: cuando una navegación devuelve un archivo adjunto (`Content-Disposition: attachment`) o un tipo binario que Firefox descarga (zip, exe, iso, 7z, rar, tar, gz, octet-stream), la respuesta se cancela y la URL se envía al host con las cookies de la sesión y el referente. El host la descarga.
- **Respaldo**: si el host no acepta en 10 segundos, la misma URL se vuelve a pedir al navegador. Nunca se pierde una descarga.
- **Menú contextual**: clic derecho sobre un enlace → *Descargar con W11DownloaderPro*.
- **Vídeo**: clic derecho sobre la página o un enlace → *Descargar vídeo de…*. El host lo resuelve con yt-dlp (sin DRM, sin HLS/DASH en esta fase) y lo descarga. Solo se hace si lo pides; las páginas no se analizan en segundo plano. Requiere una vez `w11dl-host.exe setup` (crea el entorno de yt-dlp con hashes verificados).
- **Buscar archivos en la página**: botón del popup. Lista los enlaces de archivo de la pestaña activa y envía los seleccionados.
- **Interruptor** en el popup para desactivar la captura automática.
- **Cola de descargas** en el popup: estado y progreso de cada descarga, con Pausar, Reanudar y Cancelar según corresponda. Se actualiza cada 2 segundos mientras el popup está abierto.

## Permisos y por qué se piden

| Permiso | Uso |
|---|---|
| `webRequest`, `webRequestBlocking` | Interceptar la respuesta de descargas antes de transferir nada |
| `<all_urls>` (host) | Leer cabeceras de respuesta y cookies del sitio que descarga |
| `cookies` | Enviar la sesión del usuario al host para descargas que requieren inicio de sesión |
| `downloads` | Volver a pedir la descarga al navegador si el host no acepta |
| `nativeMessaging` | Hablar con el host |
| `scripting`, `activeTab` | Leer los enlaces de la pestaña activa, solo al pulsar el botón |
| `contextMenus`, `notifications`, `storage` | Menú, avisos y preferencia del interruptor |

No se envía nada a servidores de terceros. Todo el tráfico va del navegador al host en tu equipo, y del host a los servidores que indiques.

## Pruebas

```
npm test                 # lógica, protocolo y vídeo (Node 22+; vídeo requiere W11DL_PYTHON)
FIREFOX_BIN=ruta/firefox npm run test:firefox   # extremo a extremo en Firefox real
```

`test:firefox` necesita el host compilado (`cargo build`). Cubre dos escenarios: captura con el host funcionando, y respaldo en el navegador con el host roto.

## Límites conocidos

- La captura automática solo actúa sobre **navegaciones de primer nivel**. Descargas disparadas por scripts (`fetch` + `blob:`) no se interceptan en esta versión.
- El menú contextual y el botón de buscar archivos no se han probado todavía dentro de Firefox; solo su lógica.
- La extensión necesita el host registrado. Sin él, todas las descargas siguen el camino normal del navegador.
