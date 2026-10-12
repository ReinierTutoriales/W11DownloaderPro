// Pruebas de extremo a extremo en Firefox real, mediante WebDriver BiDi.
//
// Requiere FIREFOX_BIN (binario de Firefox), el host compilado (cargo build) y Node 22+.
// Cada escenario usa un Firefox y un perfil nuevos:
//  1. captura: el host funciona. El archivo llega al host, el navegador no lo transfiere.
//  2. fallback: el host falla. El archivo se descarga en el navegador, sin pérdida.
//
// Uso: FIREFOX_BIN=/ruta/firefox node test/firefox-e2e.js
const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const FIREFOX = process.env.FIREFOX_BIN;
const HOST_BIN = process.env.W11DL_HOST || path.join(__dirname, "..", "..", "target", "debug", "w11dl-host");
const EXTENSION_DIR = process.env.EXT_DIR || path.join(__dirname, "..");
const HOST_NAME = "com.w11dl.host";
const GECKO_ID = "w11dl@reiniertutoriales";
const PAYLOAD = Buffer.from(Array.from({ length: 300_000 }, (_, i) => (i * 73) & 0xff));
const FILE = "prueba.zip";
const DEBUG_PORT = 9223;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(check, timeoutMs, label) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    const value = await check();
    if (value) return value;
    await sleep(250);
  }
  throw new Error(`tiempo agotado: ${label}`);
}

// Cliente BiDi mínimo sobre el WebSocket nativo de Node.
function bidi(url) {
  const ws = new WebSocket(url);
  let nextId = 1;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    const waiter = pending.get(msg.id);
    if (!waiter) return;
    pending.delete(msg.id);
    if (msg.type === "error") waiter.reject(new Error(`${msg.error}: ${msg.message}`));
    else waiter.resolve(msg.result);
  };
  const ready = new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = () => reject(new Error("no se pudo conectar a BiDi"));
  });
  const call = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params }));
    });
  return { ready, call, close: () => ws.close() };
}

// Un escenario completo: Firefox, perfil, host (real o roto) y un servidor que sirve el archivo.
async function scenario({ name, hostWorks }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "w11dl-ff-"));
  const home = path.join(root, "home");
  const profile = path.join(root, "profile");
  const hostOut = path.join(root, "host-out");
  const browserDl = path.join(root, "browser-dl");
  for (const d of [home, profile, hostOut, browserDl]) fs.mkdirSync(d, { recursive: true });

  // Envoltorio: el navegador no pasa argumentos al host. Si el host "no funciona", sale al instante.
  const wrapper = path.join(root, "w11dl-host-wrapper.sh");
  const body = hostWorks ? `exec "${HOST_BIN}" --out-dir "${hostOut}" "$@"` : "exit 1";
  fs.writeFileSync(wrapper, `#!/bin/sh\n${body}\n`, { mode: 0o755 });

  const nmh = path.join(home, ".mozilla", "native-messaging-hosts");
  fs.mkdirSync(nmh, { recursive: true });
  fs.writeFileSync(
    path.join(nmh, `${HOST_NAME}.json`),
    JSON.stringify({ name: HOST_NAME, description: "prueba", path: wrapper, type: "stdio", allowed_extensions: [GECKO_ID] })
  );

  const server = http.createServer((req, res) => {
    if (req.url === "/") {
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      res.end(`<a href="/${FILE}">uno</a><a href="/otro.zip">dos</a>`);
      return;
    }
    const name = path.basename(req.url.split("?")[0]);
    res.writeHead(200, {
      "Content-Type": "application/zip",
      "Content-Disposition": `attachment; filename="${name}"`,
      "Content-Length": PAYLOAD.length,
    });
    res.end(PAYLOAD);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;

  fs.writeFileSync(
    path.join(profile, "user.js"),
    [
      'user_pref("browser.download.folderList", 2);',
      `user_pref("browser.download.dir", ${JSON.stringify(browserDl)});`,
      'user_pref("browser.download.useDownloadDir", true);',
      'user_pref("browser.shell.checkDefaultBrowser", false);',
      'user_pref("browser.aboutwelcome.enabled", false);',
      'user_pref("datareporting.policy.dataSubmissionEnabled", false);',
    ].join("\n")
  );

  const ff = spawn(
    process.env.FIREFOX_BIN,
    ["--headless", "--no-remote", "--profile", profile, "--remote-debugging-port", String(DEBUG_PORT), "about:blank"],
    { cwd: root, env: { ...process.env, HOME: home }, stdio: "ignore" }
  );

  let client;
  try {
    client = await waitFor(async () => {
      const attempt = bidi(`ws://127.0.0.1:${DEBUG_PORT}/session`);
      try {
        await attempt.ready;
        return attempt;
      } catch (_) {
        attempt.close();
        return null;
      }
    }, 30000, "conexión BiDi");
    await client.call("session.new", { capabilities: {} });
    await client.call("webExtension.install", { extensionData: { type: "path", path: EXTENSION_DIR } });
    await sleep(2000); // deja cargar la configuración de la extensión

    const tree = await client.call("browsingContext.getTree", {});
    const context = tree.contexts[0].context;

    // En el escenario de captura la navegación se aborta a propósito: la extensión la cancela.
    await client
      .call("browsingContext.navigate", { context, url: `http://127.0.0.1:${port}/${FILE}`, wait: "none" })
      .catch((e) => {
        if (!/NS_ERROR_ABORT/.test(e.message)) throw e;
      });

    const target = hostWorks ? path.join(hostOut, FILE) : path.join(browserDl, FILE);
    await waitFor(() => fs.existsSync(target) && fs.statSync(target).size === PAYLOAD.length, 60000, `archivo en ${target}`);
    assert(fs.readFileSync(target).equals(PAYLOAD), "contenido distinto al original");

    await sleep(1500);
    if (hostWorks) {
      assert(!fs.existsSync(path.join(browserDl, FILE)), "el navegador completó su propia copia (no se canceló)");
    } else {
      assert(!fs.existsSync(path.join(hostOut, FILE)), "el host recibió el archivo con el host roto");
    }
    console.log(`OK ${name}: contenido verificado (${hostWorks ? "capturado por el host" : "descarga de respaldo en el navegador"})`);
  } catch (e) {
    console.error(`diagnóstico ${name} — navegador:`, fs.readdirSync(browserDl), "host:", fs.readdirSync(hostOut));
    throw e;
  } finally {
    client?.close();
    ff.kill("SIGTERM");
    server.close();
    await sleep(500);
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function assert(cond, message) {
  if (!cond) throw new Error(message);
}

async function main() {
  if (!FIREFOX) {
    console.log("omitido: define FIREFOX_BIN para ejecutar las pruebas en Firefox");
    return;
  }
  if (!fs.existsSync(HOST_BIN)) throw new Error(`host no compilado: ${HOST_BIN}`);
  await scenario({ name: "captura", hostWorks: true });
  await scenario({ name: "fallback", hostWorks: false });
}

main().catch((e) => {
  console.error("FALLO:", e.message);
  process.exitCode = 1;
});
