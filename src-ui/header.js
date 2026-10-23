// Title bar behaviour. Everything here calls a command the shell exposes; the
// page never touches the dsh webview.
(function () {
  const internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== "function") {
    return;
  }

  const invoke = (command, args) => internals.invoke(command, args);

  const bar = document.querySelector(".bar");
  const dragArea = document.querySelector(".drag");

  for (const button of document.querySelectorAll(".cap")) {
    button.addEventListener("click", () => {
      invoke("shell_window_action", { action: button.dataset.action }).catch(
        () => {}
      );
      if (button.dataset.action === "toggle-maximize") {
        syncMaximized();
      }
    });
  }

  for (const button of document.querySelectorAll(".menu")) {
    button.addEventListener("click", () => {
      const barRect = bar.getBoundingClientRect();
      const rect = button.getBoundingClientRect();
      invoke("shell_menu", {
        menu: button.dataset.menu,
        x: rect.left,
        y: barRect.height,
      }).catch(() => {});
    });
  }

  // A second press inside this window is a double click: maximize instead of
  // starting another drag. The first press already handed the window over to
  // the window manager, which is why this is decided before `shell_drag`.
  let lastPress = 0;
  dragArea.addEventListener("mousedown", (event) => {
    if (event.button !== 0) {
      return;
    }
    event.preventDefault();
    const now = Date.now();
    if (now - lastPress < 400) {
      lastPress = 0;
      invoke("shell_window_action", { action: "toggle-maximize" })
        .then(syncMaximized)
        .catch(() => {});
      return;
    }
    lastPress = now;
    invoke("shell_drag").catch(() => {});
  });

  function syncMaximized() {
    invoke("shell_window_state")
      .then((maximized) => {
        document.body.classList.toggle("maximized", !!maximized);
      })
      .catch(() => {});
  }

  syncMaximized();
})();
