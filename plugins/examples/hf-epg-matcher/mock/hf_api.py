#!/usr/bin/env python3
"""Mock of the HuggingFace Inference API (sentence-similarity) for the HF EPG Matcher demo.

    python3 -I mock/hf_api.py --port 18971 [--token TOKEN] [--down]

POST /models/<model id>   body {"inputs": {"source_sentence": "...", "sentences": ["...", ...]}}
                          -> 200 [score, ...] (one float in 0..1 per sentence, same order)
GET  /_stats              -> {"calls": N, "sentences": M}   (calls = successful model calls)
GET  /_reset              -> zero the counters

The score is deterministic: the Dice coefficient of the character trigrams of the two
normalized names (lowercase, alphanumerics, one space between words, two spaces of
padding on both ends; the better of the spaced and the space-less form), so a misnamed
channel scores highest for its true guide channel:

    "BBC Too" vs "BBC Two"  0.667     "BBC Too" vs "BBC One"  0.444

The bearer token is checked: any non-empty token is accepted, or exactly --token when
given; otherwise 401. --down answers every model call with 503 (the API being down) and
does not count it.
"""

import argparse
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Lock

STATS = {"calls": 0, "sentences": 0}
LOCK = Lock()
CONFIG = {"token": None, "down": False}


def normalize(text):
    return " ".join(re.sub(r"[^0-9a-z]+", " ", str(text).lower()).split())


def trigrams(text):
    padded = "  " + text + "  "
    return {padded[i : i + 3] for i in range(len(padded) - 2)}


def dice(left, right):
    a, b = trigrams(left), trigrams(right)
    if not left or not right:
        return 0.0
    return 2 * len(a & b) / (len(a) + len(b))


def similarity(left, right):
    """Best of the spaced and the space-less comparison, so "bbctwo" still resembles "BBC Two"."""
    left, right = normalize(left), normalize(right)
    best = max(dice(left, right), dice(left.replace(" ", ""), right.replace(" ", "")))
    return round(best, 4)


class Handler(BaseHTTPRequestHandler):
    server_version = "hf-mock/1"

    def log_message(self, fmt, *args):  # quiet: the demo output is the plugin log
        return

    def reply(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/_stats":
            with LOCK:
                return self.reply(200, dict(STATS))
        if self.path == "/_reset":
            with LOCK:
                STATS.update(calls=0, sentences=0)
            return self.reply(200, dict(STATS))
        self.reply(404, {"error": "not found"})

    def do_POST(self):
        if not self.path.startswith("/models/") or len(self.path) <= len("/models/"):
            return self.reply(404, {"error": "not found"})
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length)
        if CONFIG["down"]:
            return self.reply(503, {"error": "Service Unavailable"})
        auth = self.headers.get("authorization", "")
        token = auth[7:].strip() if auth.lower().startswith("bearer ") else ""
        if not token or (CONFIG["token"] is not None and token != CONFIG["token"]):
            return self.reply(401, {"error": "Invalid credentials in Authorization header"})
        try:
            inputs = json.loads(raw)["inputs"]
            source, sentences = inputs["source_sentence"], inputs["sentences"]
            if not isinstance(source, str) or not isinstance(sentences, list):
                raise TypeError("bad inputs")
            scores = [similarity(source, sentence) for sentence in sentences]
        except (KeyError, TypeError, ValueError):
            return self.reply(400, {"error": "expected {inputs: {source_sentence, sentences}}"})
        with LOCK:
            STATS["calls"] += 1
            STATS["sentences"] += len(sentences)
        self.reply(200, scores)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=18971)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--token", default=None, help="require exactly this bearer token (default: any non-empty)")
    parser.add_argument("--down", action="store_true", help="answer every model call with 503")
    args = parser.parse_args()
    CONFIG["token"], CONFIG["down"] = args.token, args.down
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"hf mock listening on http://{args.host}:{args.port} (down={args.down})", file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
