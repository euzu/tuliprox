#!/usr/bin/env python3
"""Mock sports API for the Sports Enricher demo. Python 3 standard library only.

    python3 -I mock/sports_api.py --port 18970 [--token demo-token] [--base UNIX] [--down | --fail]

Routes
    GET /v1/leagues                 bearer token required
    GET /v1/fixtures?league=<id>    bearer token required; one league with its fixtures
    GET /logos/<slug>.png           public (the host image cache sends no credentials)
    GET /demo/<file>                the demo playlist and guide (../demo/playlist.m3u, ../demo/guide.xml)
    GET /_stats                     call counters as JSON (?reset=1 zeroes them)

Fixture times are `--base` plus fixed offsets, so a run is deterministic. `--base` defaults to
00:00 UTC of the day the mock starts. For a frozen-clock run pass `--base 1793491200` (2026-11-01).

Modes
    --down   every route but /_stats answers 503 (the API is unreachable)
    --fail   /v1/* answers 500 (the API is up but broken); logos and demo files still work
"""

import argparse
import hashlib
import json
import os
import struct
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HOUR = 3600
DEMO_FILES = {"playlist.m3u": "audio/x-mpegurl", "guide.xml": "application/xml"}

# (id, name, sport, keywords, [(fixture id, home, away, offset seconds, venue, round, broadcasters, stats)])
LEAGUES = [
    (
        "epl", "Premier League", "football", ["premier league", "epl"],
        [
            ("epl-1001", "Liverpool", "Arsenal", 20 * HOUR + 45 * 60, "Anfield", "12",
             ["sky.sports.premierleague.uk"],
             {"home_form": "WWDWL", "away_form": "WDWWW", "head_to_head": "Liverpool 4 - 3 Arsenal (last 10)"}),
            ("epl-1002", "Chelsea", "Manchester City", 24 * HOUR + 16 * HOUR + 30 * 60, "Stamford Bridge", "12",
             ["tnt.sports.1.uk"],
             {"home_form": "LWDLW", "away_form": "WWWDW", "head_to_head": "Chelsea 2 - 5 Manchester City (last 10)"}),
            ("epl-1003", "Tottenham", "Newcastle", 24 * HOUR + 19 * HOUR, "Tottenham Hotspur Stadium", "12",
             ["sky.sports.premierleague.uk"],
             {"home_form": "DWLLW", "away_form": "WDDLW", "head_to_head": "Tottenham 3 - 3 Newcastle (last 10)"}),
            ("epl-1004", "Everton", "Aston Villa", 48 * HOUR + 15 * HOUR, "Goodison Park", "13",
             ["tnt.sports.1.uk"],
             {"home_form": "LLDWL", "away_form": "WLWDW", "head_to_head": "Everton 3 - 4 Aston Villa (last 10)"}),
        ],
    ),
    (
        "laliga", "La Liga", "football", ["la liga", "laliga"],
        [
            ("laliga-2001", "Real Madrid", "Barcelona", 24 * HOUR + 21 * HOUR, "Santiago Bernabeu", "14",
             ["laliga.tv.es"],
             {"home_form": "WWWDW", "away_form": "WLWWW", "head_to_head": "Real Madrid 5 - 4 Barcelona (last 10)"}),
            ("laliga-2002", "Sevilla", "Valencia", 48 * HOUR + 19 * HOUR, "Ramon Sanchez-Pizjuan", "14",
             ["laliga.tv.es"],
             {"home_form": "DLWDL", "away_form": "LDWLD", "head_to_head": "Sevilla 4 - 2 Valencia (last 10)"}),
            ("laliga-2003", "Atletico Madrid", "Real Betis", 72 * HOUR + 20 * HOUR, "Metropolitano", "15",
             ["laliga.tv.es"],
             {"home_form": "WDWWW", "away_form": "DWDLW", "head_to_head": "Atletico Madrid 6 - 2 Real Betis (last 10)"}),
        ],
    ),
    (
        "nba", "NBA", "basketball", ["nba"],
        [
            ("nba-3001", "Lakers", "Celtics", 24 * HOUR + 1 * HOUR, "Crypto.com Arena", "-",
             ["espn.nba.us"],
             {"home_form": "WLWWL", "away_form": "WWWWL", "head_to_head": "Lakers 6 - 4 Celtics (last 10)"}),
            ("nba-3002", "Warriors", "Bulls", 48 * HOUR + 2 * HOUR, "Chase Center", "-",
             ["espn.nba.us"],
             {"home_form": "WWLWW", "away_form": "LLWLL", "head_to_head": "Warriors 7 - 3 Bulls (last 10)"}),
            ("nba-3003", "Knicks", "Heat", 72 * HOUR + 30 * 60, "Madison Square Garden", "-",
             ["espn.nba.us"],
             {"home_form": "WLLWW", "away_form": "WWLWL", "head_to_head": "Knicks 5 - 5 Heat (last 10)"}),
        ],
    ),
]


def slug(text):
    return "".join(c if c.isalnum() else "-" for c in text.lower()).strip("-")


def png(name, size=64):
    """A deterministic solid-colour PNG derived from the name."""
    digest = hashlib.sha256(name.encode()).digest()
    row = b"\x00" + bytes(digest[0:3]) * size
    raw = row * size

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


class State:
    def __init__(self, args):
        self.args = args
        self.lock = threading.Lock()
        self.counters = {}

    def count(self, key):
        with self.lock:
            self.counters[key] = self.counters.get(key, 0) + 1

    def snapshot(self):
        with self.lock:
            counters = dict(sorted(self.counters.items()))
        api = sum(v for k, v in counters.items() if k.startswith("api:"))
        return {
            "api_calls": api,
            "unauthorized": counters.get("unauthorized", 0),
            "fixtures_calls": counters.get("api:/v1/fixtures", 0),
            "logo_requests": counters.get("logos", 0),
            "demo_requests": counters.get("demo", 0),
            "by_path": counters,
            "base": self.args.base,
            "down": self.args.down,
            "fail": self.args.fail,
        }

    def reset(self):
        with self.lock:
            self.counters.clear()


def league_payload(entry, base_url, base):
    league_id, name, sport, keywords, fixtures = entry
    return {
        "id": league_id,
        "name": name,
        "sport": sport,
        "logo": f"{base_url}/logos/{league_id}.png",
        "keywords": keywords,
        "fixtures": [
            {
                "id": fid,
                "league": league_id,
                "sport": sport,
                "home": home,
                "away": away,
                "kickoff": base + offset,
                "duration_min": 120 if sport == "football" else 150,
                "venue": venue,
                "round": rnd if rnd != "-" else "",
                "broadcasters": broadcasters,
                "home_logo": f"{base_url}/logos/{slug(home)}.png",
                "away_logo": f"{base_url}/logos/{slug(away)}.png",
                "stats": stats,
            }
            for fid, home, away, offset, venue, rnd, broadcasters, stats in fixtures
        ],
    }


def make_handler(state):
    args = state.args

    class Handler(BaseHTTPRequestHandler):
        server_version = "SportsMock/1.0"

        def log_message(self, fmt, *params):  # keep the demo terminal quiet
            return

        def send(self, status, body, content_type="application/json", headers=None):
            if isinstance(body, (dict, list)):
                body = json.dumps(body, sort_keys=True).encode()
            elif isinstance(body, str):
                body = body.encode()
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)

        def authorized(self):
            return self.headers.get("Authorization", "") == f"Bearer {args.token}"

        def base_url(self):
            return "http://" + (self.headers.get("Host") or f"127.0.0.1:{args.port}")

        def do_GET(self):
            url = urlparse(self.path)
            query = parse_qs(url.query)
            path = url.path

            if path == "/_stats":
                if query.get("reset") == ["1"]:
                    state.reset()
                return self.send(200, state.snapshot())
            if args.down:
                return self.send(503, {"error": "mock is in --down mode"})

            if path.startswith("/logos/") and path.endswith(".png"):
                name = path[len("/logos/"):-len(".png")]
                if not name or not all(c.isalnum() or c == "-" for c in name):
                    return self.send(404, {"error": "unknown logo"})
                state.count("logos")
                return self.send(200, png(name), "image/png")

            if path.startswith("/demo/"):
                name = path[len("/demo/"):]
                if name not in DEMO_FILES:
                    return self.send(404, {"error": "unknown demo file"})
                demo_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "demo")
                try:
                    with open(os.path.join(demo_dir, name), "rb") as handle:
                        data = handle.read()
                except OSError:
                    return self.send(404, {"error": "demo file missing"})
                state.count("demo")
                return self.send(200, data, DEMO_FILES[name])

            if path in ("/v1/leagues", "/v1/fixtures"):
                if not self.authorized():
                    state.count("unauthorized")
                    return self.send(401, {"error": "missing or wrong bearer token"},
                                     headers={"WWW-Authenticate": "Bearer"})
                state.count("api:" + path)
                if args.fail:
                    return self.send(500, {"error": "mock is in --fail mode"})
                if path == "/v1/leagues":
                    return self.send(200, {"leagues": [
                        {"id": e[0], "name": e[1], "sport": e[2]} for e in LEAGUES]})
                wanted = (query.get("league") or [""])[0]
                for entry in LEAGUES:
                    if entry[0] == wanted:
                        return self.send(200, league_payload(entry, self.base_url(), args.base))
                return self.send(404, {"error": f"unknown league {wanted!r}"})

            return self.send(404, {"error": "not found"})

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18970)
    parser.add_argument("--token", default="demo-token", help="the bearer token the API accepts")
    parser.add_argument("--base", type=int, default=None,
                        help="unix time the fixture offsets count from (default: 00:00 UTC today)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--down", action="store_true", help="answer 503 everywhere but /_stats")
    mode.add_argument("--fail", action="store_true", help="answer 500 on /v1/*")
    args = parser.parse_args()
    if args.base is None:
        now = int(time.time())
        args.base = now - now % 86400
    state = State(args)
    server = ThreadingHTTPServer((args.host, args.port), make_handler(state))
    print(f"sports mock on http://{args.host}:{args.port} token={args.token} base={args.base}"
          f"{' DOWN' if args.down else ''}{' FAIL' if args.fail else ''}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
