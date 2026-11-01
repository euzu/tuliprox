"""A tiny Matrix homeserver for the Matrix Notifier demo (stdlib only).

    python3 -I mock/matrix_hs.py --port 18972 [options]

It implements the one call the plugin makes, the client-server "send a room
event" endpoint:

    PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}

and behaves like a real homeserver where it matters to the plugin:

* the bearer token is checked (401 M_UNKNOWN_TOKEN) and the room is checked
  (403 M_FORBIDDEN);
* the transaction id is idempotent: a PUT with a (room, txn id) it has already
  stored answers 200 with the SAME event_id and does not store a second message;
* rate limits answer 429 with `retry_after_ms`, outages answer 5xx.

Failure modes (flags, or switched at run time with POST /_mode):

    --fail 503            answer every send with that status WITHOUT storing it
                          (the homeserver is "down")
    --fail-after-store 503
                          store the message, then answer with that status (a lost
                          acknowledgement; the retry must not duplicate)
    --rate-limit [N]      answer the first N sends (default 1) with 429
    --retry-after-ms 2000 the delay a 429 names (default 2000)

    POST /_mode {"fail": 503|null, "fail_after_store": 503|null,
                 "rate_limit": N|0, "retry_after_ms": 2000}
                          change the mode without restarting; GET /_mode shows it
    kill the process      the "connection refused" outage

It also serves a tiny fake upstream for the demo playlist, so no second mock is
needed: GET /live/<n>.ts streams 20 KiB over about 2 s, and GET /live/404.ts
answers 404 (a "dead" stream for the probe check).

Observability:

    GET /_stats           {"count": stored messages, "attempts": every send seen,
                           "rejected": {...status: n}, "txn_ids_seen": {txn: times},
                           "last": [the last 20 stored messages], "mode": {...}}
    POST /_reset          forget everything (modes included stay as started)

Data is deterministic: event ids are a hash of (room, txn id), nothing random.
"""

import argparse
import hashlib
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit

SEND = "/_matrix/client/v3/rooms/"
KEEP_LAST = 20


class State:
    def __init__(self, args):
        self.lock = threading.Lock()
        self.token = args.token
        self.room = args.room
        self.mode = {
            "fail": args.fail,
            "fail_after_store": args.fail_after_store,
            "rate_limit": args.rate_limit,
            "retry_after_ms": args.retry_after_ms,
        }
        self.reset()

    def reset(self):
        self.stored = {}  # (room, txn) -> {"event_id", "content", "seq"}
        self.attempts = 0
        self.rejected = {}
        self.txn_seen = {}
        self.rate_limited = 0

    def reject(self, status):
        self.rejected[str(status)] = self.rejected.get(str(status), 0) + 1

    def stats(self):
        ordered = sorted(self.stored.items(), key=lambda item: item[1]["seq"])
        return {
            "count": len(self.stored),
            "attempts": self.attempts,
            "rejected": self.rejected,
            "txn_ids_seen": dict(sorted(self.txn_seen.items())),
            "last": [
                {
                    "seq": value["seq"],
                    "room": room,
                    "txn_id": txn,
                    "event_id": value["event_id"],
                    "msgtype": value["content"].get("msgtype"),
                    "body": value["content"].get("body"),
                    "formatted_body": value["content"].get("formatted_body"),
                }
                for (room, txn), value in ordered[-KEEP_LAST:]
            ],
            "mode": self.mode,
        }


def event_id(room, txn):
    return "$" + hashlib.sha256(f"{room}\0{txn}".encode()).hexdigest()[:20] + ":mock"


def make_handler(state):
    class Handler(BaseHTTPRequestHandler):
        server_version = "matrix-hs-mock/1"

        def log_message(self, fmt, *args):
            sys.stderr.write("matrix_hs: " + (fmt % args) + "\n")

        def send_json(self, status, payload):
            body = json.dumps(payload, sort_keys=True).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def read_json(self):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                return json.loads(raw or b"null")
            except ValueError:
                return None

        def stream(self, path):
            if path == "/live/404.ts":
                return self.send_json(404, {"error": "no such stream"})
            chunk = bytes(range(256)) * 16  # 4 KiB, deterministic
            self.send_response(200)
            self.send_header("Content-Type", "video/mp2t")
            self.send_header("Content-Length", str(len(chunk) * 5))
            self.end_headers()
            try:
                for _ in range(5):
                    self.wfile.write(chunk)
                    self.wfile.flush()
                    time.sleep(0.4)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def do_GET(self):
            path = urlsplit(self.path).path
            with state.lock:
                if path == "/_stats":
                    return self.send_json(200, state.stats())
                if path == "/_mode":
                    return self.send_json(200, state.mode)
            if path.startswith("/live/") and path.endswith(".ts"):
                return self.stream(path)
            self.send_json(404, {"errcode": "M_NOT_FOUND", "error": "unknown route"})

        def do_POST(self):
            path = urlsplit(self.path).path
            if path == "/_reset":
                with state.lock:
                    state.reset()
                return self.send_json(200, {})
            if path == "/_mode":
                update = self.read_json()
                if not isinstance(update, dict):
                    return self.send_json(400, {"errcode": "M_BAD_JSON", "error": "expected an object"})
                with state.lock:
                    for key in state.mode:
                        if key in update:
                            state.mode[key] = update[key]
                    state.rate_limited = 0
                    return self.send_json(200, state.mode)
            self.send_json(404, {"errcode": "M_NOT_FOUND", "error": "unknown route"})

        def do_PUT(self):
            path = urlsplit(self.path).path
            content = self.read_json()
            if not path.startswith(SEND):
                return self.send_json(404, {"errcode": "M_NOT_FOUND", "error": "unknown route"})
            parts = [unquote(part) for part in path[len(SEND):].split("/")]
            if len(parts) != 4 or parts[1] != "send" or not all(parts):
                return self.send_json(404, {"errcode": "M_UNRECOGNIZED", "error": "unrecognized request"})
            room, _, event_type, txn = parts

            with state.lock:
                state.attempts += 1
                state.txn_seen[txn] = state.txn_seen.get(txn, 0) + 1
                mode = state.mode

                if self.headers.get("Authorization") != f"Bearer {state.token}":
                    state.reject(401)
                    return self.send_json(401, {"errcode": "M_UNKNOWN_TOKEN", "error": "Unrecognised access token"})
                if mode["fail"]:
                    status = int(mode["fail"])
                    state.reject(status)
                    return self.send_json(status, {"errcode": "M_UNKNOWN", "error": "homeserver unavailable"})
                if mode["rate_limit"] and state.rate_limited < int(mode["rate_limit"]):
                    state.rate_limited += 1
                    state.reject(429)
                    return self.send_json(
                        429,
                        {
                            "errcode": "M_LIMIT_EXCEEDED",
                            "error": "Too many requests",
                            "retry_after_ms": int(mode["retry_after_ms"]),
                        },
                    )
                if room != state.room:
                    state.reject(403)
                    return self.send_json(403, {"errcode": "M_FORBIDDEN", "error": "not a member of that room"})
                if event_type != "m.room.message" or not isinstance(content, dict):
                    state.reject(400)
                    return self.send_json(400, {"errcode": "M_BAD_JSON", "error": "expected an m.room.message object"})

                key = (room, txn)
                if key not in state.stored:  # idempotent on the transaction id
                    state.stored[key] = {"event_id": event_id(room, txn), "content": content, "seq": len(state.stored) + 1}
                eid = state.stored[key]["event_id"]
                if mode["fail_after_store"]:
                    status = int(mode["fail_after_store"])
                    state.reject(status)
                    return self.send_json(status, {"errcode": "M_UNKNOWN", "error": "lost the acknowledgement"})
                return self.send_json(200, {"event_id": eid})

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--port", type=int, default=18972)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--token", default="demo-token", help="the bearer token the homeserver accepts")
    parser.add_argument("--room", default="!demo:localhost", help="the only room it knows")
    parser.add_argument("--fail", type=int, default=None, metavar="STATUS")
    parser.add_argument("--fail-after-store", type=int, default=None, metavar="STATUS")
    parser.add_argument("--rate-limit", type=int, nargs="?", const=1, default=0, metavar="N")
    parser.add_argument("--retry-after-ms", type=int, default=2000)
    args = parser.parse_args()

    state = State(args)
    server = ThreadingHTTPServer((args.host, args.port), make_handler(state))
    print(f"matrix_hs: listening on http://{args.host}:{args.port} (room {args.room}, token {args.token})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
