// Botón "Descargar vídeo" sobre los vídeos de la página.
//
// No toca el DOM de la página salvo un único contenedor propio (Shadow DOM cerrado) que se mueve
// sobre el vídeo bajo el puntero. Sin movimiento del puntero no hace nada, y nunca descarga por sí
// solo: solo un clic real del usuario envía el vídeo al acelerador.
(function () {
  "use strict";
  if (window.__w11dlVideoButton) return;
  window.__w11dlVideoButton = true;

  const core = self.W11DL;
  const SETTING = "videoButton";
  const HIDE_AFTER_MS = 2500;
  const LABEL = "⬇ Descargar vídeo";

  let enabled = true;
  browser.storage.local.get(SETTING).then((s) => {
    enabled = s[SETTING] !== false;
  });
  browser.storage.onChanged.addListener((changes) => {
    if (SETTING in changes) {
      enabled = changes[SETTING].newValue !== false;
      if (!enabled) hide();
    }
  });

  // Vídeos que han pedido claves de cifrado (DRM). `encrypted` no sube por el árbol: se captura.
  const protectedVideos = new WeakSet();
  document.addEventListener("encrypted", (e) => protectedVideos.add(e.target), true);

  const isProtected = (video) => video.mediaKeys != null || protectedVideos.has(video);

  // ---- Interfaz: un contenedor, un botón ----
  const host = document.createElement("div");
  host.style.cssText = "all:initial;position:fixed;left:0;top:0;width:0;height:0;z-index:2147483647;pointer-events:none;";
  const shadow = host.attachShadow({ mode: "closed" });
  const style = document.createElement("style");
  style.textContent = `
    button {
      all: initial; box-sizing: border-box; position: fixed; display: none;
      width: ${core.BUTTON_WIDTH}px; height: ${core.BUTTON_HEIGHT}px; line-height: ${core.BUTTON_HEIGHT}px;
      text-align: center; font: 600 12px system-ui, sans-serif; color: #fff;
      background: rgba(20, 20, 24, .88); border: 1px solid rgba(255,255,255,.35); border-radius: 15px;
      cursor: pointer; pointer-events: auto; user-select: none;
    }
    button:hover { background: rgba(0, 110, 220, .95); }
    button.on { display: block; }
  `;
  const button = document.createElement("button");
  button.textContent = LABEL;
  button.type = "button";
  shadow.append(style, button);

  let active = null; // vídeo sobre el que está el botón
  let overButton = false;
  let hideTimer = null;
  let pointer = null;
  let frame = 0;

  function mount() {
    // En pantalla completa solo se pinta lo que cuelga del elemento a pantalla completa.
    const parent = document.fullscreenElement || document.documentElement;
    if (host.parentNode !== parent) parent.append(host);
  }

  function hide() {
    clearTimeout(hideTimer);
    button.classList.remove("on");
    active = null;
  }

  function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      if (!overButton) hide();
    }, HIDE_AFTER_MS);
  }

  function describe(video) {
    const rect = video.getBoundingClientRect();
    const style = getComputedStyle(video);
    const visible = style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity) !== 0;
    return {
      rect,
      state: core.videoButtonState({
        width: rect.width,
        height: rect.height,
        visible,
        loop: video.loop,
        muted: video.muted,
        drm: isProtected(video),
      }),
    };
  }

  function place(video) {
    const { rect } = describe(video);
    const pos = core.buttonPosition(rect, { width: innerWidth, height: innerHeight });
    if (!pos) return hide();
    mount();
    button.style.left = `${pos.left}px`;
    button.style.top = `${pos.top}px`;
    button.classList.add("on");
    active = video;
  }

  function update() {
    frame = 0;
    if (!enabled || !pointer) return;
    // `getElementsByTagName` es una colección viva: no hay nada que mantener ni observar.
    for (const video of document.getElementsByTagName("video")) {
      const { rect, state } = describe(video);
      if (state.show && core.pointInRect(pointer.x, pointer.y, rect)) {
        place(video);
        scheduleHide();
        return;
      }
    }
    if (active && !overButton) scheduleHide();
  }

  function schedule() {
    if (!frame) frame = requestAnimationFrame(update);
  }

  document.addEventListener(
    "pointermove",
    (e) => {
      if (!enabled) return;
      pointer = { x: e.clientX, y: e.clientY };
      schedule();
    },
    { passive: true, capture: true }
  );
  const reposition = () => {
    if (active) place(active);
  };
  addEventListener("scroll", reposition, { passive: true, capture: true });
  addEventListener("resize", reposition, { passive: true });
  document.addEventListener("fullscreenchange", () => {
    if (active) {
      mount();
      place(active);
    }
  });

  button.addEventListener("pointerenter", () => {
    overButton = true;
    clearTimeout(hideTimer);
  });
  button.addEventListener("pointerleave", () => {
    overButton = false;
    scheduleHide();
  });
  // El reproductor de la página no debe enterarse de los clics sobre nuestro botón.
  for (const type of ["pointerdown", "pointerup", "mousedown", "mouseup", "dblclick"]) {
    button.addEventListener(type, (e) => e.stopPropagation());
  }

  button.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    // Un script de la página no puede fabricar este clic.
    if (!e.isTrusted || !active) return;
    const target = core.videoTarget({
      currentSrc: active.currentSrc,
      frameUrl: location.href,
      pageUrl: document.referrer,
    });
    if (!target) {
      flash("No disponible");
      return;
    }
    flash("Enviando…");
    try {
      const reply = await browser.runtime.sendMessage(core.videoClickMessage(target));
      flash(reply && reply.ok ? "En cola ✓" : "No se pudo");
    } catch (_) {
      flash("No se pudo");
    }
  });

  let flashTimer = null;
  function flash(text) {
    button.textContent = text;
    clearTimeout(flashTimer);
    flashTimer = setTimeout(() => {
      button.textContent = LABEL;
    }, 2500);
  }
})();
