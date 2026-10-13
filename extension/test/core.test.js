const { test } = require("node:test");
const assert = require("node:assert/strict");
const core = require("../lib/core.js");

test("cabeceras solo con lista blanca y valores seguros", () => {
  const headers = core.buildHeaders({
    cookies: [{ name: "sid", value: "abc" }, { name: "x", value: "a\r\nInjected: 1" }],
    referrer: "https://ejemplo.test/pagina",
    userAgent: "Agente/1.0",
  });
  assert.deepEqual(headers, {
    Cookie: "sid=abc",
    Referer: "https://ejemplo.test/pagina",
    "User-Agent": "Agente/1.0",
  });
});

test("referente no web se descarta", () => {
  assert.equal(core.buildHeaders({ referrer: "file:///etc/passwd" }).Referer, undefined);
});

test("sin cookies no se envía cabecera Cookie", () => {
  assert.equal(core.buildHeaders({ cookies: [] }).Cookie, undefined);
});

test("nombre sugerido tiene prioridad y se limpia", () => {
  assert.equal(core.fileNameFrom("https://x.test/a.zip", 'mal<nombre>:?.zip'), "mal_nombre___.zip");
});

test("una ruta completa sugerida por el navegador da solo el nombre de archivo", () => {
  assert.equal(core.fileNameFrom("https://x.test/a.zip", "/tmp/descargas/prueba.zip"), "prueba.zip");
  assert.equal(core.fileNameFrom("https://x.test/a.zip", "C:\\Users\\yo\\Downloads\\prueba.zip"), "prueba.zip");
});

test("sin nombre sugerido se usa el último segmento decodificado", () => {
  assert.equal(core.fileNameFrom("https://x.test/carpeta/mi%20archivo.pdf?x=1"), "mi archivo.pdf");
});

test("sin nombre utilizable cae a un nombre por defecto", () => {
  assert.equal(core.fileNameFrom("https://x.test/"), "archivo");
  assert.equal(core.fileNameFrom("https://x.test/", "..."), "archivo");
});

test("intercepta solo navegaciones que son descargas, con captura activa", () => {
  const attach = { type: "main_frame", method: "GET", url: "https://x.test/a.zip", responseHeaders: [{ name: "Content-Disposition", value: 'attachment; filename="a.zip"' }] };
  const binary = { type: "main_frame", method: "GET", url: "https://x.test/b.bin", responseHeaders: [{ name: "content-type", value: "application/octet-stream" }] };
  assert.equal(core.shouldIntercept(attach, true), true);
  assert.equal(core.shouldIntercept(binary, true), true);
  assert.equal(core.shouldIntercept(attach, false), false);
  assert.equal(core.shouldIntercept(attach, undefined), false, "sin configuración cargada no se captura");
  assert.equal(core.shouldIntercept({ ...attach, type: "image" }, true), false, "solo navegaciones de primer nivel");
  assert.equal(core.shouldIntercept({ ...attach, url: "blob:https://x.test/1" }, true), false);
});

test("una descarga que nace de un POST (u otro método) no se intercepta", () => {
  const attach = { type: "main_frame", method: "GET", url: "https://x.test/a.zip", responseHeaders: [{ name: "Content-Disposition", value: "attachment" }] };
  assert.equal(core.shouldIntercept(attach, true), true);
  for (const method of ["POST", "PUT", "HEAD", "post", "", undefined, null, 5]) {
    assert.equal(core.shouldIntercept({ ...attach, method }, true), false, `método ${String(method)}`);
  }
  assert.equal(core.shouldIntercept({ ...attach, method: "get" }, true), true, "el método no distingue mayúsculas");
  const sinMetodo = { ...attach };
  delete sinMetodo.method;
  assert.equal(core.shouldIntercept(sinMetodo, true), false, "sin método conocido no se intercepta");
  assert.equal(core.isGetRequest(null), false);
});

test("un PDF que se muestra en la pestaña no se intercepta sin adjunto", () => {
  const inline = { type: "main_frame", method: "GET", url: "https://x.test/d.pdf", responseHeaders: [{ name: "Content-Type", value: "application/pdf" }] };
  assert.equal(core.shouldIntercept(inline, true), false);
  const attached = { ...inline, responseHeaders: [...inline.responseHeaders, { name: "Content-Disposition", value: "attachment" }] };
  assert.equal(core.shouldIntercept(attached, true), true);
});

test("nombre propuesto por el servidor: filename y filename*", () => {
  assert.equal(core.filenameFromDisposition('attachment; filename="informe final.pdf"'), "informe final.pdf");
  assert.equal(core.filenameFromDisposition("attachment; filename*=UTF-8''informe%20%C3%B1.pdf"), "informe ñ.pdf");
  assert.equal(core.filenameFromDisposition("inline"), "");
});

test("detección de enlaces de archivo por extensión", () => {
  assert.equal(core.isFileLink("https://x.test/y.PDF?z=1"), true);
  assert.equal(core.isFileLink("https://x.test/pagina.html"), false);
  assert.equal(core.isFileLink("mailto:a@b.test"), false);
});

test("mensaje de encolado tiene la forma que espera el host", () => {
  const msg = core.enqueueMessage({ id: "1", url: "https://x.test/a", filename: "a", headers: undefined });
  assert.deepEqual(msg, { type: "enqueue", id: "1", url: "https://x.test/a", filename: "a", headers: {} });
});

test("mensaje de vídeo no lleva cabeceras de sesión", () => {
  assert.deepEqual(core.enqueueVideoMessage({ id: "9", page: "https://x.test/v" }), {
    type: "enqueue_video",
    id: "9",
    page: "https://x.test/v",
  });
});

test("acciones de la cola según el estado", () => {
  assert.deepEqual(core.queueActions("running"), ["pause", "cancel"]);
  assert.deepEqual(core.queueActions("paused"), ["resume", "cancel"]);
  assert.deepEqual(core.queueActions("interrupted"), ["resume", "cancel"]);
  assert.deepEqual(core.queueActions("failed"), ["resume", "cancel"]);
  assert.deepEqual(core.queueActions("done"), []);
  assert.deepEqual(core.queueActions("canceled"), []);
});

test("mensaje de límite: consulta, valor válido y valores inválidos", () => {
  assert.deepEqual(core.limitMessage(), { type: "set_limit" });
  assert.deepEqual(core.limitMessage(0), { type: "set_limit", bytes_per_sec: 0 });
  assert.deepEqual(core.limitMessage(1048576), { type: "set_limit", bytes_per_sec: 1048576 });
  for (const bad of [-1, 1.5, NaN, Infinity, "5", null]) assert.equal(core.limitMessage(bad), null);
  assert.equal(core.LIMIT_CHOICES[0].value, 0);
});

test("descarga en espera: el mensaje lo marca y las acciones permiten iniciarla", () => {
  assert.equal(core.enqueueMessage({ id: "1", url: "https://x.test/a", filename: "a", queued: true }).queued, true);
  assert.equal("queued" in core.enqueueMessage({ id: "1", url: "https://x.test/a", filename: "a" }), false);
  assert.deepEqual(core.queueActions("queued"), ["resume", "cancel"]);
});

test("botón de vídeo: solo para vídeos reales y sin DRM", () => {
  const base = { width: 640, height: 360, visible: true, loop: false, muted: false, drm: false };
  assert.equal(core.videoButtonState(base).show, true);
  assert.deepEqual(core.videoButtonState({ ...base, drm: true }), { show: false, reason: "drm" });
  assert.equal(core.videoButtonState({ ...base, visible: false }).show, false);
  assert.equal(core.videoButtonState({ ...base, width: 150 }).show, false);
  assert.equal(core.videoButtonState({ ...base, height: 100 }).show, false);
  assert.equal(core.videoButtonState({ ...base, loop: true, muted: true }).reason, "decorativo");
  assert.equal(core.videoButtonState({ ...base, loop: true, muted: false }).show, true);
  assert.equal(core.videoButtonState({ ...base, width: undefined }).show, false);
});

test("destino del vídeo: archivo directo o página para yt-dlp", () => {
  const page = "https://sitio.test/ver?v=1";
  assert.deepEqual(core.videoTarget({ currentSrc: "https://cdn.test/a/clip.mp4?t=1", pageUrl: page }), {
    kind: "direct",
    url: "https://cdn.test/a/clip.mp4?t=1",
  });
  // blob:, data: y listas de reproducción no son archivos.
  for (const src of ["blob:https://sitio.test/uuid", "data:video/mp4;base64,AAAA", "https://cdn.test/live.m3u8", ""]) {
    assert.deepEqual(core.videoTarget({ currentSrc: src, pageUrl: page }), { kind: "page", url: page });
  }
  // En un iframe se prefiere la URL del propio reproductor.
  assert.deepEqual(
    core.videoTarget({ currentSrc: "blob:x", frameUrl: "https://player.test/embed/9", pageUrl: page }),
    { kind: "page", url: "https://player.test/embed/9" }
  );
  assert.equal(core.videoTarget({ currentSrc: "blob:x", frameUrl: "about:blank", pageUrl: "file:///x" }), null);
});

test("posición del botón: esquina superior derecha, dentro de la ventana", () => {
  const vp = { width: 1000, height: 700 };
  const rect = { left: 100, top: 50, right: 740, bottom: 410 };
  const pos = core.buttonPosition(rect, vp);
  assert.deepEqual(pos, { left: 740 - core.BUTTON_WIDTH - 8, top: 58 });
  // Vídeo que sobresale por la derecha: el botón queda dentro de la ventana.
  const wide = core.buttonPosition({ left: 100, top: 50, right: 1400, bottom: 410 }, vp);
  assert.equal(wide.left, 1000 - core.BUTTON_WIDTH - 8);
  // Parcialmente arriba: el botón baja al borde visible.
  assert.equal(core.buttonPosition({ left: 0, top: -200, right: 640, bottom: 160 }, vp).top, 8);
  // Fuera de la ventana o casi sin altura visible: sin botón.
  assert.equal(core.buttonPosition({ left: 0, top: 800, right: 640, bottom: 1100 }, vp), null);
  assert.equal(core.buttonPosition({ left: 1200, top: 0, right: 1800, bottom: 300 }, vp), null);
  assert.equal(core.buttonPosition({ left: 0, top: -190, right: 640, bottom: -160 }, vp), null);
  assert.equal(core.pointInRect(150, 100, rect), true);
  assert.equal(core.pointInRect(50, 100, rect), false);
});

test("el fondo valida los clics de vídeo y rechaza todo lo demás", () => {
  const ok = core.videoClickMessage({ kind: "page", url: "https://sitio.test/v" });
  assert.equal(core.validVideoClick(ok), true);
  for (const bad of [
    null,
    {},
    { ...ok, kind: "otro" },
    { ...ok, url: "file:///etc/passwd" },
    { ...ok, url: "javascript:alert(1)" },
    { ...ok, url: "https://x.test/" + "a".repeat(5000) },
    { ...ok, type: "queue-action" },
  ]) {
    assert.equal(core.validVideoClick(bad), false);
  }
});
