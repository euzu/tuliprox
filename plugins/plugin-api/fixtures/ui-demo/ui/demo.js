// Shared by every ui-demo frame page. No inline scripts: the frame CSP is
// `script-src 'self'`, and `connect-src 'none'` blocks fetch.
(async () => {
  const tuliprox = window.tuliprox;
  const $ = (id) => document.getElementById(id);
  const show = (id, value) => {
    $(id).textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  };
  const describe = (error) => {
    if (error && typeof error === "object" && !(error instanceof Error)) return JSON.stringify(error);
    return String(error);
  };

  await tuliprox.ready;

  const render = () => {
    show("slot", tuliprox.slot);
    show("locale", tuliprox.locale);
    show("hello", tuliprox.t("hello"));
    show("context", tuliprox.context === undefined ? "undefined" : tuliprox.context);
    tuliprox.resize();
  };
  render();
  for (const event of ["init", "locale", "theme", "context"]) tuliprox.on(event, render);

  const call = async (method, payload) => {
    try {
      show("rpc-out", `${method}: ${JSON.stringify(await tuliprox.rpc(method, payload))}`);
    } catch (error) {
      show("rpc-out", `${method} rejected: ${describe(error)}`);
    }
  };
  $("btn-echo").addEventListener("click", () => call("echo", { message: "ping" }));
  $("btn-bump").addEventListener("click", () => call("bump", { by: 1 }));
  $("btn-toast").addEventListener("click", () => tuliprox.toast("success", tuliprox.t("toast.msg")));

  // Each probe is expected to throw or reject inside the sandbox.
  $("btn-probe").addEventListener("click", async () => {
    const lines = [];
    const attempt = async (label, action) => {
      try {
        const value = await action();
        lines.push(`${label}: NOT BLOCKED -> ${JSON.stringify(value)}`);
      } catch (error) {
        const name = error && error.name ? error.name : "error";
        const message = error && error.message ? error.message : String(error);
        lines.push(`${label}: blocked (${name}: ${message})`);
      }
    };
    await attempt('localStorage.getItem("x")', () => localStorage.getItem("x"));
    await attempt("parent.document.title", () => parent.document.title);
    await attempt('fetch("/api/v1/status")', async () => (await fetch("/api/v1/status")).status);
    show("probe-out", lines.join("\n"));
    tuliprox.resize();
  });
})();
