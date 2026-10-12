// Script de fondo (página de evento no persistente en Firefox).
//
// Captura: se intercepta la respuesta de una navegación que es una descarga (adjunto o tipo binario)
// y se cancela antes de transferir nada. Se entrega la URL al host nativo, que la descarga.
// Si el host no acepta, la misma URL se vuelve a pedir al navegador, sin pérdida.
//
// Enlaces: el menú contextual y el popup envían URLs de archivos al host sin interceptar nada.

const HOST = "com.w11dl.host";
const HANDOFF_TIMEOUT_MS = 10000;
const ENABLED_KEY = "captureEnabled";
const MAX_LINKS_PER_SEND = 200;
const BYPASS_TTL_MS = 30000;

// null: configuración aún no cargada. Mientras sea null no se captura nada.
let captureEnabled = null;
// URLs que el navegador debe descargar tal cual (fallback o reenvío propio). Evita bucles.
const bypass = new Set();

let port = null;
// id -> { resolve, reject }: esperan el acuse `accepted` o un `error` de esa petición.
const waiting = new Map();

function ensurePort() {
  if (port) return port;
  port = browser.runtime.connectNative(HOST);
  port.onMessage.addListener(onHostMessage);
  port.onDisconnect.addListener((p) => {
    const reason = (p.error && p.error.message) || "el host nativo no está disponible";
    for (const pending of waiting.values()) pending.reject(new Error(reason));
    waiting.clear();
    port = null;
  });
  return port;
}

function onHostMessage(msg) {
  if (!msg || typeof msg !== "object") return;
  switch (msg.type) {
    case "accepted": {
      const pending = waiting.get(msg.id);
      if (pending) {
        waiting.delete(msg.id);
        pending.resolve(msg);
      }
      break;
    }
    case "error": {
      const pending = waiting.get(msg.id);
      if (pending) {
        waiting.delete(msg.id);
        pending.reject(new Error(msg.message));
      } else {
        setBadge("!");
        notify("Error al descargar", msg.message);
      }
      break;
    }
    case "done":
      setBadge("");
      notify("Descarga completada", msg.path);
      break;
    default:
      // progress y pong no requieren acción aquí.
      break;
  }
}

// Envía una descarga al host y espera su acuse. Rechaza si no acepta en el plazo.
function handoff({ url, filename, headers }) {
  return new Promise((resolve, reject) => {
    const id = self.W11DL.newId();
    const timer = setTimeout(() => {
      waiting.delete(id);
      reject(new Error("el host no respondió"));
    }, HANDOFF_TIMEOUT_MS);
    waiting.set(id, {
      resolve: (msg) => {
        clearTimeout(timer);
        resolve(msg);
      },
      reject: (err) => {
        clearTimeout(timer);
        reject(err);
      },
    });
    try {
      ensurePort().postMessage(self.W11DL.enqueueMessage({ id, url, filename, headers }));
    } catch (err) {
      clearTimeout(timer);
      waiting.delete(id);
      port = null;
      reject(err);
    }
  });
}

// Cabeceras de sesión: cookies del navegador para esa URL y el referente, si lo hay.
async function sessionHeaders(url, referrer) {
  let cookies = [];
  try {
    cookies = await browser.cookies.getAll({ url });
  } catch (_) {
    // Sin permiso o sin cookies: se descarga sin sesión.
  }
  return self.W11DL.buildHeaders({ cookies, referrer, userAgent: navigator.userAgent });
}

async function getEnabled() {
  const stored = await browser.storage.local.get(ENABLED_KEY);
  return stored[ENABLED_KEY] !== false;
}

function setBadge(text) {
  browser.action.setBadgeText({ text }).catch(() => {});
}

function notify(title, message) {
  browser.notifications
    .create({ type: "basic", title, message: String(message).slice(0, 200) })
    .catch(() => {});
}

// Descarga que el navegador debe volver a pedir: la marcamos para no interceptarla de nuevo.
function fallbackToBrowser(url, filename) {
  bypass.add(url);
  setTimeout(() => bypass.delete(url), BYPASS_TTL_MS);
  return browser.downloads.download({ url, filename });
}

// Entrega una navegación interceptada al host. Si el host no acepta, el navegador la descarga.
async function captureNavigation(details) {
  const { url } = details;
  const suggested = self.W11DL.filenameFromDisposition(
    self.W11DL.headerValue(details.responseHeaders, "content-disposition")
  );
  const filename = self.W11DL.fileNameFrom(url, suggested);
  try {
    const headers = await sessionHeaders(url, details.originUrl);
    await handoff({ url, filename, headers });
    notify("Capturada", filename);
  } catch (err) {
    setBadge("!");
    notify("Se descargará en el navegador", err.message);
    await fallbackToBrowser(url, filename).catch((e) => notify("Error de descarga", e.message));
  }
}

browser.storage.local.get(ENABLED_KEY).then((stored) => {
  captureEnabled = stored[ENABLED_KEY] !== false;
});

browser.storage.onChanged.addListener((changes) => {
  if (ENABLED_KEY in changes) captureEnabled = changes[ENABLED_KEY].newValue !== false;
});

// Bloqueante y síncrono: decide solo con los datos de la respuesta. El trabajo pesado va después.
browser.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (bypass.delete(details.url)) return {};
    if (!self.W11DL.shouldIntercept(details, captureEnabled)) return {};
    captureNavigation(details);
    return { cancel: true };
  },
  { urls: ["<all_urls>"], types: ["main_frame"] },
  ["blocking", "responseHeaders"]
);

browser.contextMenus.onClicked.addListener(async (info) => {
  if (info.menuItemId !== "w11dl-link" || !info.linkUrl) return;
  await sendOne(info.linkUrl, info.pageUrl);
});

async function sendOne(url, pageUrl) {
  if (!self.W11DL.isWebUrl(url)) return { url, ok: false, error: "URL no web" };
  try {
    const headers = await sessionHeaders(url, pageUrl);
    await handoff({ url, filename: self.W11DL.fileNameFrom(url), headers });
    return { url, ok: true };
  } catch (err) {
    setBadge("!");
    return { url, ok: false, error: err.message };
  }
}

browser.runtime.onInstalled.addListener(() => {
  browser.contextMenus.create({
    id: "w11dl-link",
    title: "Descargar con W11DownloaderPro",
    contexts: ["link"],
  });
});

browser.runtime.onMessage.addListener(async (msg) => {
  switch (msg && msg.type) {
    case "get-enabled":
      return { enabled: await getEnabled() };
    case "set-enabled":
      await browser.storage.local.set({ [ENABLED_KEY]: msg.enabled === true });
      return { ok: true };
    case "send-links": {
      const urls = Array.isArray(msg.urls) ? msg.urls.slice(0, MAX_LINKS_PER_SEND) : [];
      const results = [];
      // Secuencial: el host ya paraleliza las descargas; enviar en ráfaga solo llenaría su cola.
      for (const url of urls) results.push(await sendOne(url, msg.pageUrl));
      return { results };
    }
    default:
      return { error: "mensaje desconocido" };
  }
});
