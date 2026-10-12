// Prueba del botón de vídeo en un navegador real (Chromium con Playwright). Valida la lógica del
// script de contenido sobre un DOM y un diseño reales; NO es Firefox ni una extensión instalada.
// Se omite si Playwright no está disponible. Uso: node test/video-button.dom.js
const fs = require("node:fs");
const path = require("node:path");
const assert = require("node:assert/strict");

let chromium;
try {
  ({ chromium } = require("playwright"));
} catch (_) {
  console.log("omitido: Playwright no está instalado");
  process.exit(0);
}

const core = fs.readFileSync(path.join(__dirname, "..", "lib", "core.js"), "utf8");
const content = fs.readFileSync(path.join(__dirname, "..", "content", "video-button.js"), "utf8");

// Sustituto mínimo de la API `browser` que el script necesita.
const shim = () => {
  window.__sent = [];
  window.__docClicks = 0;
  window.browser = {
    storage: { local: { get: async () => ({}) }, onChanged: { addListener() {} } },
    runtime: { sendMessage: async (m) => (window.__sent.push(m), { ok: true }) },
  };
  document.addEventListener("click", () => window.__docClicks++);
};

const PAGE = (video) => `<!doctype html><body style="margin:0;height:2000px">
  <video id="v" ${video} style="position:absolute;left:100px;top:50px"></video></body>`;

async function open(browser, videoAttrs) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 } });
  await page.addInitScript(shim);
  await page.goto("http://localhost/", { waitUntil: "commit" }).catch(() => {});
  await page.route("http://localhost/**", (r) => r.fulfill({ contentType: "text/html", body: PAGE(videoAttrs) }));
  await page.goto("http://localhost/pagina");
  await page.addScriptTag({ content: core });
  await page.addScriptTag({ content: content });
  return page;
}

const sent = (page) => page.evaluate(() => window.__sent);
const wait = (page, ms) => page.waitForTimeout(ms);
// Centro del botón para un vídeo en (100,50) de 640x360: esquina superior derecha.
const BTN = { x: 100 + 640 - 132 - 8 + 66, y: 50 + 8 + 15 };

(async () => {
  const browser = await chromium.launch();
  try {
    // 1. Vídeo normal: hover muestra el botón y el clic envía la página a yt-dlp.
    let page = await open(browser, 'width="640" height="360"');
    assert.equal((await sent(page)).length, 0, "nada se envía sin un clic en el botón");
    await page.mouse.move(300, 200);
    await wait(page, 150);
    await page.mouse.move(BTN.x, BTN.y);
    await page.evaluate(() => (window.__docClicks = 0));
    await page.mouse.click(BTN.x, BTN.y);
    await wait(page, 100);
    const messages = await sent(page);
    assert.equal(messages.length, 1, JSON.stringify(messages));
    assert.deepEqual(messages[0], { type: "video-click", kind: "page", url: "http://localhost/pagina" });
    assert.equal(await page.evaluate(() => window.__docClicks), 0, "la página no debe ver el clic del botón");
    console.log("ok 1: hover + clic envía el vídeo y la página no se entera");

    // 2. Con scroll el botón sigue al vídeo.
    await page.mouse.move(300, 200);
    await wait(page, 100);
    await page.evaluate(() => window.scrollBy(0, 30));
    await wait(page, 150);
    await page.mouse.click(BTN.x, BTN.y - 30);
    await wait(page, 100);
    assert.equal((await sent(page)).length, 2, "tras el scroll el botón debía seguir al vídeo");
    console.log("ok 2: el botón sigue al vídeo al hacer scroll");

    // 3. Pasados unos segundos sin actividad, el botón desaparece.
    await page.mouse.move(900, 600);
    await wait(page, 2800);
    await page.mouse.click(BTN.x, BTN.y - 30);
    await wait(page, 100);
    assert.equal((await sent(page)).length, 2, "el botón debía haberse ocultado");
    console.log("ok 3: se oculta tras unos segundos");
    await page.close();

    // 4. Vídeo pequeño, decorativo y con DRM: no hay botón.
    for (const [name, attrs, setup] of [
      ["pequeño", 'width="150" height="100"', null],
      ["decorativo", 'width="640" height="360" loop muted', null],
      ["con DRM", 'width="640" height="360"', () => document.getElementById("v").dispatchEvent(new Event("encrypted"))],
    ]) {
      page = await open(browser, attrs);
      if (setup) await page.evaluate(setup);
      await page.mouse.move(300, 200);
      await wait(page, 150);
      await page.mouse.click(BTN.x, BTN.y);
      await wait(page, 100);
      assert.equal((await sent(page)).length, 0, `vídeo ${name}: no debía haber botón`);
      console.log(`ok 4: sin botón en vídeo ${name}`);
      await page.close();
    }

    console.log("TODO OK (Chromium, no Firefox)");
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
