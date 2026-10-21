"""Apple TV control for the ButterBot plane, with pyatv.

Run by the plane (bbplane::appletv) in its own virtual environment. Speaks one JSON object a line:
requests {"id": n, "cmd": ..., ...} on stdin, replies {"id": n, "ok": true, "result": ...} or
{"id": n, "ok": false, "error": ...} on stdout, and events {"event": "playing", ...} whenever what
the Apple TV plays changes. Pairing credentials live in the pyatv storage file it is given.
"""

import argparse
import asyncio
import json
import sys

import pyatv
from pyatv.const import DeviceState, FeatureName, FeatureState, MediaType, PowerState, Protocol
from pyatv.interface import PowerListener, PushListener
from pyatv.storage.file_storage import FileStorage

PROTOCOLS = {"companion": Protocol.Companion, "airplay": Protocol.AirPlay}


def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def playing_json(p, app=None):
    return {
        "state": p.device_state.name.lower() if p.device_state else "idle",
        "media_type": p.media_type.name.lower() if p.media_type else "unknown",
        "title": p.title,
        "artist": p.artist,
        "album": p.album,
        "series": p.series_name,
        "position": p.position,
        "total": p.total_time,
        "app": app.name if app else None,
    }


class Listener(PushListener, PowerListener):
    def __init__(self, svc):
        self.svc = svc

    def playstatus_update(self, updater, playstatus):
        app = None
        try:
            app = self.svc.atv.metadata.app
        except Exception:
            pass
        out({"event": "playing", **playing_json(playstatus, app)})

    def playstatus_error(self, updater, exception):
        out({"event": "error", "error": str(exception)})

    def powerstate_update(self, old_state, new_state):
        out({"event": "power", "on": new_state == PowerState.On})

    def connection_lost(self, exception):
        out({"event": "disconnected", "error": str(exception)})
        self.svc.atv = None

    def connection_closed(self):
        out({"event": "disconnected"})
        self.svc.atv = None


class Service:
    def __init__(self, storage_path):
        self.loop = asyncio.get_running_loop()
        self.storage = FileStorage(storage_path, self.loop)
        self.atv = None
        self.conf = None
        self.pairing = None
        self.listener = Listener(self)

    async def find(self, address=None, identifier=None, timeout=5):
        hosts = [address] if address else None
        ids = {identifier} if identifier else None
        found = await pyatv.scan(self.loop, hosts=hosts, identifier=ids, storage=self.storage, timeout=timeout)
        return found

    async def cmd_scan(self, req):
        found = await self.find(req.get("address"), None, req.get("timeout", 5))
        return [
            {
                "name": c.name,
                "address": str(c.address),
                "id": c.identifier,
                "model": c.device_info.model_str if c.device_info else None,
                "services": [
                    {"protocol": s.protocol.name.lower(), "paired": bool(s.credentials), "pairing": s.pairing.name.lower()}
                    for s in c.services
                ],
            }
            for c in found
        ]

    async def cmd_pair_start(self, req):
        found = await self.find(req.get("address"), req.get("id"))
        if not found:
            raise RuntimeError("no Apple TV found there")
        self.conf = found[0]
        if self.pairing:
            await self.pairing.close()
        self.pairing = await pyatv.pair(self.conf, PROTOCOLS[req["protocol"]], self.loop, storage=self.storage, name="Butterbot")
        await self.pairing.begin()
        return {"device_provides_pin": self.pairing.device_provides_pin}

    async def cmd_pair_pin(self, req):
        if not self.pairing:
            raise RuntimeError("no pairing in progress")
        self.pairing.pin(int(req["pin"]))
        await self.pairing.finish()
        ok = self.pairing.has_paired
        await self.pairing.close()
        self.pairing = None
        await self.storage.save()
        return {"paired": ok}

    async def cmd_connect(self, req):
        if self.atv:
            self.atv.close()
            self.atv = None
        await self.storage.load()
        found = await self.find(req.get("address"), req.get("id"))
        if not found:
            raise RuntimeError("no Apple TV found there")
        self.conf = found[0]
        self.atv = await pyatv.connect(self.conf, self.loop, storage=self.storage)
        self.atv.listener = self.listener
        try:
            self.atv.push_updater.listener = self.listener
            self.atv.push_updater.start()
        except Exception as e:
            out({"event": "error", "error": f"no live updates: {e}"})
        try:
            self.atv.power.listener = self.listener
        except Exception:
            pass
        return {"name": self.conf.name}

    def need(self):
        if not self.atv:
            raise RuntimeError("not connected to the Apple TV")
        return self.atv

    def available(self, feature):
        try:
            return self.atv.features.get_feature(feature).state == FeatureState.Available
        except Exception:
            return False

    async def cmd_status(self, req):
        atv = self.need()
        result = {"name": self.conf.name, "connected": True}
        try:
            app = atv.metadata.app if self.available(FeatureName.App) else None
            result["playing"] = playing_json(await atv.metadata.playing(), app)
        except Exception as e:
            result["playing_error"] = str(e)
        try:
            result["on"] = atv.power.power_state == PowerState.On
        except Exception:
            pass
        try:
            result["volume"] = atv.audio.volume if self.available(FeatureName.Volume) else None
        except Exception:
            pass
        return result

    async def cmd_control(self, req):
        atv = self.need()
        action = req["action"]
        rc = atv.remote_control
        simple = {
            "play": rc.play,
            "pause": rc.pause,
            "play_pause": rc.play_pause,
            "stop": rc.stop,
            "next": rc.next,
            "previous": rc.previous,
            "volume_up": rc.volume_up,
            "volume_down": rc.volume_down,
            "home": rc.home,
            "menu": rc.menu,
            "select": rc.select,
            "up": rc.up,
            "down": rc.down,
            "left": rc.left,
            "right": rc.right,
            "turn_on": atv.power.turn_on,
            "turn_off": atv.power.turn_off,
        }
        if action in simple:
            await simple[action]()
            return {"done": action}
        if action == "set_volume":
            await atv.audio.set_volume(float(req["value"]))
            return {"volume": float(req["value"])}
        raise RuntimeError(f"unknown action {action}")

    async def cmd_apps(self, req):
        atv = self.need()
        return [{"name": a.name, "id": a.identifier} for a in await atv.apps.app_list()]

    async def cmd_launch(self, req):
        atv = self.need()
        want = req["app"].strip().lower()
        apps = await atv.apps.app_list()
        match = next((a for a in apps if a.identifier.lower() == want), None) or next(
            (a for a in apps if a.name.lower() == want), None
        ) or next((a for a in apps if want in a.name.lower()), None)
        if not match:
            raise RuntimeError(f"no app called {req['app']}")
        await atv.apps.launch_app(match.identifier)
        return {"launched": match.name}


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--storage", required=True)
    args = parser.parse_args()

    svc = Service(args.storage)
    await svc.storage.load()
    reader = asyncio.StreamReader()
    await svc.loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
    out({"event": "ready", "pyatv": pyatv.const.__version__ if hasattr(pyatv.const, "__version__") else None})

    async def handle(req):
        rid = req.get("id")
        try:
            fn = getattr(svc, "cmd_" + str(req.get("cmd")), None)
            if fn is None:
                raise RuntimeError(f"unknown command {req.get('cmd')}")
            out({"id": rid, "ok": True, "result": await fn(req)})
        except Exception as e:
            out({"id": rid, "ok": False, "error": str(e) or type(e).__name__})

    while True:
        line = await reader.readline()
        if not line:
            break
        try:
            req = json.loads(line)
        except ValueError:
            continue
        asyncio.ensure_future(handle(req))
    if svc.atv:
        svc.atv.close()


if __name__ == "__main__":
    asyncio.run(main())
