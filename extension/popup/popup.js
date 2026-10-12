const $ = (id) => document.getElementById(id);
const status = (text) => { $("status").textContent = text; };

async function init() {
  const { enabled } = await browser.runtime.sendMessage({ type: "get-enabled" });
  $("enabled").checked = enabled;
}

$("enabled").addEventListener("change", (e) =>
  browser.runtime.sendMessage({ type: "set-enabled", enabled: e.target.checked })
);

$("scan").addEventListener("click", async () => {
  const [tab] = await browser.tabs.query({ active: true, currentWindow: true });
  if (!tab || !tab.url || !W11DL.isWebUrl(tab.url)) {
    status("Esta página no se puede analizar.");
    return;
  }
  // Lee los enlaces del DOM de la pestaña activa. Solo se ejecuta al pulsar el botón.
  const [injected] = await browser.scripting.executeScript({
    target: { tabId: tab.id },
    func: () => Array.from(document.querySelectorAll("a[href]"), (a) => a.href),
  });
  const urls = [...new Set((injected.result || []).filter((u) => W11DL.isFileLink(u)))];
  const list = $("links");
  list.replaceChildren();
  for (const url of urls) {
    const label = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = url;
    box.checked = true;
    label.append(box, url);
    list.append(label);
  }
  $("send").hidden = urls.length === 0;
  status(urls.length ? `${urls.length} archivos encontrados.` : "No se encontraron archivos.");
  $("send").onclick = async () => {
    const selected = [...list.querySelectorAll("input:checked")].map((b) => b.value);
    const { results } = await browser.runtime.sendMessage({
      type: "send-links",
      urls: selected,
      pageUrl: tab.url,
    });
    const ok = results.filter((r) => r.ok).length;
    status(`Enviados: ${ok} de ${results.length}.`);
  };
});

init();

// ---- Cola de descargas ----

const LABELS = {
  running: "En curso",
  paused: "Pausada",
  interrupted: "Interrumpida (reanudable)",
  failed: "Fallida",
  done: "Terminada",
  canceled: "Cancelada",
};
const ACTION_LABELS = { pause: "Pausar", resume: "Reanudar", cancel: "Cancelar" };

function formatMb(bytes) {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function renderQueue(items) {
  const list = $("queue");
  list.replaceChildren();
  $("queue-empty").hidden = items.length > 0;
  for (const item of items) {
    const row = document.createElement("div");
    row.className = "job";
    const name = document.createElement("div");
    name.className = "name";
    name.textContent = item.path.split(/[\\/]/).pop();
    const meta = document.createElement("div");
    meta.className = "meta";
    meta.textContent = `${LABELS[item.state] || item.state} · ${formatMb(item.bytes || 0)}`;
    row.append(name, meta);
    for (const action of W11DL.queueActions(item.state)) {
      const button = document.createElement("button");
      button.textContent = ACTION_LABELS[action];
      button.addEventListener("click", async () => {
        const result = await browser.runtime.sendMessage({ type: "queue-action", action, id: item.id });
        status(result.ok ? "Orden enviada." : `No se pudo: ${result.error}`);
        refreshQueue();
      });
      row.append(button);
    }
    list.append(row);
  }
}

async function refreshQueue() {
  const { items, error } = await browser.runtime.sendMessage({ type: "queue-list" });
  if (error) {
    status(`Cola no disponible: ${error}`);
    return;
  }
  renderQueue(items);
}

$("queue-refresh").addEventListener("click", refreshQueue);
refreshQueue();
setInterval(refreshQueue, 2000);
