// Prueba de extremo a extremo: lanza el host nativo real y le habla con el mismo framing
// que usa el navegador (longitud de 4 bytes little-endian + JSON).
const { test } = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");
const core = require("../lib/core.js");

const HOST_BIN = process.env.W11DL_HOST || path.join(__dirname, "..", "..", "target", "debug", process.platform === "win32" ? "w11dl-host.exe" : "w11dl-host");

function serveFile(data) {
  const server = http.createServer((req, res) => {
    const range = /^bytes=(\d+)-(\d+)$/.exec(req.headers.range || "");
    if (range) {
      const start = Number(range[1]);
      const end = Math.min(Number(range[2]), data.length - 1);
      res.writeHead(206, {
        "Content-Type": "application/octet-stream",
        "Content-Range": `bytes ${start}-${end}/${data.length}`,
        "Content-Length": end - start + 1,
        ETag: '"v1"',
      });
      res.end(data.subarray(start, end + 1));
    } else {
      res.writeHead(200, { "Content-Type": "application/octet-stream", "Content-Length": data.length, ETag: '"v1"' });
      res.end(data);
    }
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

// Cliente de framing: escribe mensajes y lee respuestas por bloques.
function connect(outDir) {
  const child = spawn(HOST_BIN, ["--out-dir", outDir], { stdio: ["pipe", "pipe", "pipe"] });
  let buffer = Buffer.alloc(0);
  const queue = [];
  const waiters = [];
  child.stdout.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 4) {
      const len = buffer.readUInt32LE(0);
      if (buffer.length < 4 + len) break;
      const msg = JSON.parse(buffer.subarray(4, 4 + len).toString("utf8"));
      buffer = buffer.subarray(4 + len);
      if (waiters.length) waiters.shift()(msg);
      else queue.push(msg);
    }
  });
  const next = () =>
    new Promise((resolve, reject) => {
      if (queue.length) return resolve(queue.shift());
      const timer = setTimeout(() => reject(new Error("tiempo agotado esperando al host")), 15000);
      waiters.push((msg) => {
        clearTimeout(timer);
        resolve(msg);
      });
    });
  const send = (obj) => {
    const body = Buffer.from(JSON.stringify(obj), "utf8");
    const header = Buffer.alloc(4);
    header.writeUInt32LE(body.length, 0);
    child.stdin.write(Buffer.concat([header, body]));
  };
  const close = () => new Promise((resolve) => {
    child.on("exit", (code) => resolve(code));
    child.stdin.end();
  });
  return { send, next, close, child };
}

test("el host acepta, descarga y entrega el archivo exacto", async (t) => {
  if (!fs.existsSync(HOST_BIN)) {
    t.skip(`host no compilado: ${HOST_BIN} (ejecuta cargo build)`);
    return;
  }
  const data = Buffer.from(Array.from({ length: 700_000 }, (_, i) => (i * 37) & 0xff));
  const server = await serveFile(data);
  const outDir = fs.mkdtempSync(path.join(os.tmpdir(), "w11dl-e2e-"));
  const url = `http://127.0.0.1:${server.address().port}/archivo.bin`;
  const client = connect(outDir);
  try {
    client.send({ type: "ping" });
    assert.equal((await client.next()).type, "pong");

    client.send(core.enqueueMessage({
      id: "job-1",
      url,
      filename: core.fileNameFrom(url, "capturado.bin"),
      headers: core.buildHeaders({ referrer: "https://pagina.test/", userAgent: "Prueba/1.0" }),
    }));

    const accepted = await client.next();
    assert.equal(accepted.type, "accepted");
    assert.equal(accepted.id, "job-1");

    let done;
    for (;;) {
      const msg = await client.next();
      if (msg.type === "progress") continue;
      done = msg;
      break;
    }
    assert.equal(done.type, "done", JSON.stringify(done));
    assert.deepEqual(fs.readFileSync(path.join(outDir, "capturado.bin")), data);
  } finally {
    server.close();
    await client.close();
    fs.rmSync(outDir, { recursive: true, force: true });
  }
});

test("una cabecera fuera de la lista blanca se rechaza antes de descargar", async (t) => {
  if (!fs.existsSync(HOST_BIN)) {
    t.skip(`host no compilado: ${HOST_BIN}`);
    return;
  }
  const outDir = fs.mkdtempSync(path.join(os.tmpdir(), "w11dl-e2e-"));
  const client = connect(outDir);
  try {
    client.send({
      type: "enqueue",
      id: "bad",
      url: "http://127.0.0.1:9/x.bin",
      headers: { "X-Evil": "1" },
    });
    const reply = await client.next();
    assert.equal(reply.type, "error");
    assert.match(reply.message, /no permitida/);
  } finally {
    await client.close();
    fs.rmSync(outDir, { recursive: true, force: true });
  }
});

test("vídeo de página: el host resuelve con yt-dlp, acusa y descarga", async (t) => {
  if (!fs.existsSync(HOST_BIN)) {
    t.skip(`host no compilado: ${HOST_BIN}`);
    return;
  }
  if (!process.env.W11DL_PYTHON) {
    t.skip("define W11DL_PYTHON con un Python que tenga yt-dlp");
    return;
  }
  const data = Buffer.from(Array.from({ length: 250_000 }, (_, i) => (i * 11) & 0xff));
  const page = `<!doctype html><html><head><title>Prueba</title></head><body><video src="/clip.mp4"></video></body></html>`;
  const server = http.createServer((req, res) => {
    if (req.url === "/clip.mp4") {
      res.writeHead(200, { "Content-Type": "video/mp4", "Content-Length": data.length });
      res.end(data);
    } else {
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      res.end(page);
    }
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const outDir = fs.mkdtempSync(path.join(os.tmpdir(), "w11dl-video-"));
  const client = connect(outDir);
  try {
    client.send(core.enqueueVideoMessage({ id: "vid", page: `http://127.0.0.1:${server.address().port}/` }));
    assert.equal((await client.next()).type, "probing");
    const accepted = await client.next();
    assert.equal(accepted.type, "accepted", JSON.stringify(accepted));
    let done;
    for (;;) {
      const msg = await client.next();
      if (msg.type === "progress") continue;
      done = msg;
      break;
    }
    assert.equal(done.type, "done", JSON.stringify(done));
    assert.deepEqual(fs.readFileSync(path.join(outDir, path.basename(accepted.path))), data);
  } finally {
    server.close();
    await client.close();
    fs.rmSync(outDir, { recursive: true, force: true });
  }
});

test("el host real fija, recuerda y devuelve el límite con los mensajes de la extensión", async (t) => {
  if (!fs.existsSync(HOST_BIN)) {
    t.skip(`host no compilado: ${HOST_BIN} (ejecuta cargo build)`);
    return;
  }
  const outDir = fs.mkdtempSync(path.join(os.tmpdir(), "w11dl-limit-"));
  let client = connect(outDir);
  try {
    client.send(core.limitMessage());
    assert.deepEqual(await client.next(), { type: "limit", bytes_per_sec: 0 });
    client.send(core.limitMessage(1048576));
    assert.deepEqual(await client.next(), { type: "limit", bytes_per_sec: 1048576 });
    await client.close();

    client = connect(outDir);
    client.send(core.limitMessage());
    assert.deepEqual(await client.next(), { type: "limit", bytes_per_sec: 1048576 });
  } finally {
    await client.close();
  }
});
