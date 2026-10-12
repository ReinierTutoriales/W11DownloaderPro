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
  const attach = { type: "main_frame", url: "https://x.test/a.zip", responseHeaders: [{ name: "Content-Disposition", value: 'attachment; filename="a.zip"' }] };
  const binary = { type: "main_frame", url: "https://x.test/b.bin", responseHeaders: [{ name: "content-type", value: "application/octet-stream" }] };
  assert.equal(core.shouldIntercept(attach, true), true);
  assert.equal(core.shouldIntercept(binary, true), true);
  assert.equal(core.shouldIntercept(attach, false), false);
  assert.equal(core.shouldIntercept(attach, undefined), false, "sin configuración cargada no se captura");
  assert.equal(core.shouldIntercept({ ...attach, type: "image" }, true), false, "solo navegaciones de primer nivel");
  assert.equal(core.shouldIntercept({ ...attach, url: "blob:https://x.test/1" }, true), false);
});

test("un PDF que se muestra en la pestaña no se intercepta sin adjunto", () => {
  const inline = { type: "main_frame", url: "https://x.test/d.pdf", responseHeaders: [{ name: "Content-Type", value: "application/pdf" }] };
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
