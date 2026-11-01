"use strict";
// Compiled from ui/menu.ts (vanilla TypeScript, ES2020). Keep the two in step.
const bridge = window.tuliprox;
function byId(id) {
    const element = document.getElementById(id);
    if (!element) {
        throw new Error(`missing element #${id}`);
    }
    return element;
}
function render() {
    byId("title").textContent = bridge.t("menu.title");
    byId("hint").textContent = bridge.t("menu.hint");
    byId("slot").textContent = bridge.slot;
    byId("locale").textContent = bridge.locale;
    bridge.resize();
}
async function main() {
    await bridge.ready;
    render();
    for (const event of ["init", "locale"]) {
        bridge.on(event, render);
    }
    byId("toast").addEventListener("click", () => bridge.toast("success", bridge.t("menu.toast")));
}
void main();
