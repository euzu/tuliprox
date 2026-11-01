#!/usr/bin/env python3
"""Offline Xtream Codes provider for the Sports Enricher VOD metadata check. Standard library only.

    python3 -I mock/xtream_provider.py --port 18973

Serves `GET /player_api.php?username=demo&password=demo[&action=...]`: login, live and VOD categories
and streams, `get_vod_info`, and empty series lists. It holds three VOD titles: two sports
documentaries and one that is not about sport. `GET /_stats` counts the calls per action.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

USER, PASSWORD = "demo", "demo"
ADDED = "1700000000"

LIVE_CATEGORIES = [{"category_id": "1", "category_name": "Provider Sports", "parent_id": 0}]
LIVE = [
    {"num": 1, "name": "Eurosport Tennis HD", "stream_type": "live", "stream_id": 501, "stream_icon": "",
     "epg_channel_id": "eurosport.tennis", "added": ADDED, "category_id": "1", "custom_sid": "",
     "tv_archive": 0, "direct_source": "", "tv_archive_duration": 0},
]
VOD_CATEGORIES = [{"category_id": "10", "category_name": "Documentaries", "parent_id": 0}]
VOD = {
    101: ("Anfield Stories: A Football Documentary", "Documentary",
          "A season behind the scenes at a Premier League club.", "7.4"),
    102: ("Courtside: The NBA Documentary", "Documentary",
          "Five seasons of the league's best basketball, told by the players.", "8.1"),
    103: ("Pasta Masters", "Documentary", "Three chefs travel Italy in search of the perfect noodle.", "6.9"),
}


def vod_stream(stream_id):
    name, _genre, _plot, rating = VOD[stream_id]
    return {
        "num": stream_id, "name": name, "stream_type": "movie", "stream_id": stream_id, "stream_icon": "",
        "rating": rating, "rating_5based": round(float(rating) / 2, 1), "added": ADDED, "category_id": "10",
        "container_extension": "mp4", "custom_sid": "", "direct_source": "",
    }


def vod_info(stream_id):
    name, genre, plot, rating = VOD[stream_id]
    return {
        "info": {
            "kinopoisk_url": "", "tmdb_id": "", "name": name, "o_name": name, "cover_big": "", "movie_image": "",
            "releasedate": "2024-05-12", "episode_run_time": "90", "youtube_trailer": "", "director": "",
            "actors": "", "cast": "", "description": "", "plot": plot, "age": "", "mpaa_rating": "",
            "rating_count_kinopoisk": 0, "country": "UK", "genre": genre, "backdrop_path": [],
            "duration_secs": 5400, "duration": "01:30:00", "video": [], "audio": [], "bitrate": 0,
            "rating": rating,
        },
        "movie_data": {
            "stream_id": stream_id, "name": name, "added": ADDED, "category_id": "10",
            "container_extension": "mp4", "custom_sid": "", "direct_source": "",
        },
    }


class State:
    def __init__(self):
        self.lock = threading.Lock()
        self.counters = {}

    def count(self, key):
        with self.lock:
            self.counters[key] = self.counters.get(key, 0) + 1

    def snapshot(self):
        with self.lock:
            return {"by_action": dict(sorted(self.counters.items()))}


def make_handler(state, port):
    class Handler(BaseHTTPRequestHandler):
        server_version = "XtreamMock/1.0"

        def log_message(self, fmt, *params):
            return

        def send(self, status, payload):
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            url = urlparse(self.path)
            query = {k: v[0] for k, v in parse_qs(url.query).items()}
            if url.path == "/_stats":
                return self.send(200, state.snapshot())
            if url.path != "/player_api.php":
                return self.send(404, {"error": "not found"})
            if query.get("username") != USER or query.get("password") != PASSWORD:
                state.count("unauthorized")
                return self.send(401, {"user_info": {"auth": 0}})
            action = query.get("action", "login")
            state.count(action)
            if action == "login":
                now = int(time.time())
                return self.send(200, {
                    "user_info": {"username": USER, "password": PASSWORD, "message": "", "auth": 1,
                                  "status": "Active", "exp_date": "1900000000", "is_trial": "0",
                                  "active_cons": "0", "created_at": "1600000000", "max_connections": "2",
                                  "allowed_output_formats": ["m3u8", "ts"]},
                    "server_info": {"url": "127.0.0.1", "port": str(port), "https_port": "",
                                    "server_protocol": "http", "rtmp_port": "", "timezone": "UTC",
                                    "timestamp_now": now, "time_now": time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(now))},
                })
            if action == "get_live_categories":
                return self.send(200, LIVE_CATEGORIES)
            if action == "get_live_streams":
                return self.send(200, LIVE)
            if action == "get_vod_categories":
                return self.send(200, VOD_CATEGORIES)
            if action == "get_vod_streams":
                return self.send(200, [vod_stream(i) for i in sorted(VOD)])
            if action == "get_vod_info":
                try:
                    stream_id = int(query.get("vod_id", ""))
                except ValueError:
                    return self.send(404, [])
                if stream_id not in VOD:
                    return self.send(404, [])
                return self.send(200, vod_info(stream_id))
            return self.send(200, [])

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18973)
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), make_handler(State(), args.port))
    print(f"xtream mock on http://{args.host}:{args.port} user={USER} password={PASSWORD}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
