// Lógica pura de la extensión: sin APIs del navegador, para probarla con Node.
// Se carga como script clásico en Firefox (define W11DL) y como módulo CommonJS en las pruebas.
(function (root) {
  "use strict";

  // Cabeceras que el host acepta. Deben coincidir con la lista del host (src/native.rs).
  const ALLOWED_HEADERS = ["Cookie", "Referer", "User-Agent", "Accept-Language"];

  // Extensiones de archivo que se capturan desde la página.
  const FILE_EXTENSIONS = [
    "zip", "7z", "rar", "tar", "gz", "exe", "msi", "iso", "pdf", "docx", "xlsx", "pptx",
    "mp4", "mkv", "webm", "mp3", "flac",
  ];

  const MAX_FILENAME = 200;

  function isWebUrl(value) {
    try {
      const url = new URL(value);
      return url.protocol === "http:" || url.protocol === "https:";
    } catch (_) {
      return false;
    }
  }

  // Un valor de cabecera no puede contener saltos de línea ni NUL: evita inyección de cabeceras.
  function isSafeHeaderValue(value) {
    return typeof value === "string" && !/[\r\n\0]/.test(value);
  }

  function cookieHeader(cookies) {
    return (cookies || [])
      .filter((c) => c && typeof c.name === "string" && c.name && isSafeHeaderValue(c.value))
      .map((c) => `${c.name}=${c.value}`)
      .join("; ");
  }

  // Cabeceras de sesión para una descarga. Solo las de la lista blanca.
  function buildHeaders({ cookies, referrer, userAgent }) {
    const headers = {};
    const cookie = cookieHeader(cookies);
    if (cookie && isSafeHeaderValue(cookie)) headers.Cookie = cookie;
    if (referrer && isWebUrl(referrer)) headers.Referer = referrer;
    if (userAgent && isSafeHeaderValue(userAgent)) headers["User-Agent"] = userAgent;
    return headers;
  }

  // Nombre de archivo para una URL: el sugerido por el navegador si existe; si no, el último
  // segmento de la ruta. Quita caracteres prohibidos en Windows.
  function fileNameFrom(url, suggested) {
    // El navegador puede sugerir una ruta completa: solo interesa el último componente.
    let name = typeof suggested === "string" ? suggested.split(/[\\/]/).pop() : "";
    if (!name) {
      try {
        const segments = new URL(url).pathname.split("/");
        name = decodeURIComponent(segments[segments.length - 1] || "");
      } catch (_) {
        name = "";
      }
    }
    const clean = name
      .replace(/[<>:"/\\|?*\u0000-\u001f]/g, "_")
      .replace(/^[\s.]+|[\s.]+$/g, "")
      .slice(0, MAX_FILENAME);
    return clean || "archivo";
  }

  function extensionOf(name) {
    const dot = name.lastIndexOf(".");
    return dot > 0 && dot < name.length - 1 ? name.slice(dot + 1).toLowerCase() : "";
  }

  function isFileLink(url) {
    try {
      const path = new URL(url).pathname;
      return FILE_EXTENSIONS.includes(extensionOf(path.split("/").pop() || ""));
    } catch (_) {
      return false;
    }
  }

  // Tipos que el navegador descarga sin mostrarlos. PDF, vídeo y audio se reproducen en la
  // pestaña, así que solo se capturan cuando el servidor los marca explícitamente como adjuntos.
  const DOWNLOAD_TYPES = [
    "application/zip",
    "application/x-zip-compressed",
    "application/x-7z-compressed",
    "application/x-rar-compressed",
    "application/gzip",
    "application/x-tar",
    "application/octet-stream",
    "application/x-msdownload",
    "application/x-msdos-program",
    "application/x-iso9660-image",
  ];

  function headerValue(headers, name) {
    const found = (headers || []).find((h) => h && typeof h.name === "string" && h.name.toLowerCase() === name);
    return found && typeof found.value === "string" ? found.value : "";
  }

  // Nombre que propone el servidor en Content-Disposition (RFC 6266, con o sin filename*).
  function filenameFromDisposition(value) {
    const extended = /filename\*\s*=\s*[^']*'[^']*'([^;]+)/i.exec(value || "");
    if (extended) {
      try {
        return decodeURIComponent(extended[1].trim().replace(/^"|"$/g, ""));
      } catch (_) {
        return "";
      }
    }
    const plain = /filename\s*=\s*"?([^";]+)"?/i.exec(value || "");
    return plain ? plain[1].trim() : "";
  }

  // Indica si una respuesta de navegación es una descarga que debemos capturar.
  function isDownloadResponse(details) {
    if (!details || details.type !== "main_frame") return false;
    const disposition = headerValue(details.responseHeaders, "content-disposition");
    if (/^\s*attachment\b/i.test(disposition)) return true;
    const type = headerValue(details.responseHeaders, "content-type").split(";")[0].trim().toLowerCase();
    return DOWNLOAD_TYPES.includes(type);
  }

  // Decide si una respuesta debe interceptarse. `enabled` debe ser `true` explícitamente:
  // mientras la configuración no ha cargado, no se captura nada.
  function shouldIntercept(details, enabled) {
    return enabled === true && isWebUrl(details && details.url) && isDownloadResponse(details);
  }

  function newId() {
    return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
  }

  // `queued`: solo registrarla en la cola del host, sin empezar (la inicia el usuario o la cola programada).
  function enqueueMessage({ id, url, filename, headers, queued }) {
    const message = { type: "enqueue", id, url, filename, headers: headers || {} };
    if (queued === true) message.queued = true;
    return message;
  }

  // Página o enlace de vídeo: el host la resuelve con yt-dlp. Sin cabeceras de sesión.
  // Acciones válidas sobre una descarga según su estado (espejo de `State::is_resumable` en el host).
  function queueActions(state) {
    switch (state) {
      case "running":
        return ["pause", "cancel"];
      case "queued":
      case "paused":
      case "interrupted":
      case "failed":
        return ["resume", "cancel"];
      default:
        return [];
    }
  }

  // Opciones del selector de límite de velocidad (bytes/s; 0 = sin límite).
  const LIMIT_CHOICES = [
    { label: "Sin límite", value: 0 },
    { label: "256 KB/s", value: 256 * 1024 },
    { label: "1 MB/s", value: 1024 * 1024 },
    { label: "5 MB/s", value: 5 * 1024 * 1024 },
    { label: "20 MB/s", value: 20 * 1024 * 1024 },
  ];

  // Mensaje al host para fijar el límite; sin valor solo lo consulta. Rechaza lo que no sea un entero >= 0.
  function limitMessage(bytesPerSec) {
    if (bytesPerSec === undefined) return { type: "set_limit" };
    if (!Number.isSafeInteger(bytesPerSec) || bytesPerSec < 0) return null;
    return { type: "set_limit", bytes_per_sec: bytesPerSec };
  }

  // ---- Botón sobre los vídeos ----

  // Contenedores que el motor descarga como archivo directo (espejo de DIRECT_VIDEO_EXTS en el host).
  const DIRECT_VIDEO_EXTENSIONS = ["mp4", "webm", "mkv", "mov"];
  const MIN_VIDEO_WIDTH = 200;
  const MIN_VIDEO_HEIGHT = 120;
  const BUTTON_WIDTH = 132;
  const BUTTON_HEIGHT = 30;
  const BUTTON_MARGIN = 8;

  // ¿Merece un botón este vídeo? Excluye los pequeños, los ocultos, los decorativos (en bucle y sin
  // sonido, típicos de fondos de portada) y los que usan DRM, que no se pueden descargar.
  function videoButtonState({ width, height, visible, loop, muted, drm }) {
    if (drm) return { show: false, reason: "drm" };
    if (!visible) return { show: false, reason: "oculto" };
    if (!(width >= MIN_VIDEO_WIDTH) || !(height >= MIN_VIDEO_HEIGHT)) return { show: false, reason: "pequeño" };
    if (loop && muted) return { show: false, reason: "decorativo" };
    return { show: true, reason: "" };
  }

  // Qué se envía al host: el archivo directo si el vídeo lo es, o la página para que yt-dlp la resuelva.
  // `blob:`, `data:` y el streaming por segmentos no son archivos: solo sirve la página.
  function videoTarget({ currentSrc, frameUrl, pageUrl }) {
    if (isWebUrl(currentSrc)) {
      const ext = extensionOf(new URL(currentSrc).pathname.split("/").pop() || "");
      if (DIRECT_VIDEO_EXTENSIONS.includes(ext)) return { kind: "direct", url: currentSrc };
    }
    for (const candidate of [frameUrl, pageUrl]) {
      if (isWebUrl(candidate)) return { kind: "page", url: candidate };
    }
    return null;
  }

  // Esquina superior derecha del vídeo, dentro de la ventana. `null` si el vídeo no se ve.
  function buttonPosition(rect, viewport) {
    const left = Math.min(rect.right, viewport.width) - BUTTON_WIDTH - BUTTON_MARGIN;
    const top = Math.max(rect.top, 0) + BUTTON_MARGIN;
    const visibleHeight = Math.min(rect.bottom, viewport.height) - Math.max(rect.top, 0);
    if (rect.right <= 0 || rect.left >= viewport.width || visibleHeight < BUTTON_HEIGHT + BUTTON_MARGIN) {
      return null;
    }
    return { left: Math.max(left, Math.max(rect.left, 0) + BUTTON_MARGIN), top };
  }

  function pointInRect(x, y, rect) {
    return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
  }

  // Mensaje del script de contenido al fondo. El fondo lo valida de nuevo: no se fía del remitente.
  function videoClickMessage(target) {
    return { type: "video-click", kind: target.kind, url: target.url };
  }

  function validVideoClick(msg) {
    return Boolean(
      msg &&
        msg.type === "video-click" &&
        (msg.kind === "direct" || msg.kind === "page") &&
        isWebUrl(msg.url) &&
        msg.url.length <= 4096
    );
  }

  function enqueueVideoMessage({ id, page }) {
    return { type: "enqueue_video", id, page };
  }

  const api = {
    ALLOWED_HEADERS,
    FILE_EXTENSIONS,
    isWebUrl,
    cookieHeader,
    buildHeaders,
    fileNameFrom,
    extensionOf,
    isFileLink,
    headerValue,
    filenameFromDisposition,
    isDownloadResponse,
    shouldIntercept,
    newId,
    enqueueMessage,
    enqueueVideoMessage,
    queueActions,
    LIMIT_CHOICES,
    limitMessage,
    videoButtonState,
    videoTarget,
    buttonPosition,
    pointInRect,
    videoClickMessage,
    validVideoClick,
    BUTTON_WIDTH,
    BUTTON_HEIGHT,
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    root.W11DL = api;
  }
})(typeof self !== "undefined" ? self : globalThis);
