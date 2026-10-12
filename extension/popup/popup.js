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
