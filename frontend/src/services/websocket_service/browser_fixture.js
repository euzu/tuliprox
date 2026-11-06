// Synchronous browser tests own the globals only for the lifetime of this fixture.
let restore;
let timers;
let now;
let sample;
let failConstructor;
let failSend;
let statusResponses;

export function installFixture() {
    restore?.();
    const originals = {
        WebSocket: globalThis.WebSocket,
        setTimeout: globalThis.setTimeout,
        clearTimeout: globalThis.clearTimeout,
        fetch: globalThis.fetch,
    };
    const random = Math.random;
    const clock = Object.getOwnPropertyDescriptor(performance, "now");
    let nextTimer = 0;
    timers = new Map();
    now = 0;
    sample = 0.5;
    failConstructor = false;
    failSend = false;
    statusResponses = [];
    globalThis.WebSocket = class extends EventTarget {
        constructor() {
            super();
            if (failConstructor) throw new DOMException("Fixture constructor failure", "SyntaxError");
            this.readyState = 0;
            this.sent = [];
        }
        dispatchEvent(event) {
            this["on" + event.type]?.call(this, event);
            return super.dispatchEvent(event);
        }
        send(bytes) {
            if (failSend) throw new DOMException("Fixture send failure", "InvalidStateError");
            this.sent.push(bytes.slice());
        }
        close() { this.readyState = 3; }
    };
    globalThis.setTimeout = (callback, delay) => {
        const id = --nextTimer;
        timers.set(id, { at: now + delay, callback });
        return id;
    };
    globalThis.clearTimeout = (id) => {
        if (id < 0) timers.delete(id);
        else originals.clearTimeout.call(globalThis, id);
    };
    globalThis.fetch = (request, ...options) => {
        if (new URL(request.url || request, location.href).pathname === "/api/v1/status") {
            return new Promise((resolve) => statusResponses.push(resolve));
        }
        return originals.fetch.call(globalThis, request, ...options);
    };
    Object.defineProperty(performance, "now", { configurable: true, value: () => now });
    Math.random = () => sample;
    restore = () => {
        Object.assign(globalThis, originals);
        if (clock) Object.defineProperty(performance, "now", clock);
        else delete performance.now;
        Math.random = random;
        restore = undefined;
    };
}

export function restoreFixture() { restore?.(); }
export function rejectConstructor(reject) { failConstructor = reject; }
export function rejectSend(reject) { failSend = reject; }
export function jitterSample(value) { sample = value; }
export function timerCount() { return timers.size; }
export function nextDelay() { return Math.min(...Array.from(timers.values(), (timer) => timer.at)) - now; }
export function jumpClock(ms) { now += ms; }
export function advanceClock(ms) {
    const end = now + ms;
    while (true) {
        const next = Array.from(timers).filter(([, timer]) => timer.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        const [id, timer] = next;
        now = Math.max(now, timer.at);
        timers.delete(id);
        timer.callback();
    }
    now = end;
}

export function openSocket(socket) {
    socket.readyState = 1;
    socket.dispatchEvent(new Event("open"));
}
export function sentCount(socket) { return socket.sent.length; }
export function sentFrame(socket, index) { return socket.sent[index]; }
export function resolveStatus(index, json) {
    statusResponses[index](new Response(json, { headers: { "Content-Type": "application/json" } }));
}
