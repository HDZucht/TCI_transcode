#!/usr/bin/env python3
"""Check kit/tci-kit.toml against its own [rules] and say exactly what is wrong.

Ends:       a misconfigured kit is stopped before anything starts, with one line per fault that names the rule, the
            decoders involved and the value to change; a configuration msdr would refuse never reaches msdr.
Ways:       reads the file with tomllib; the numbers come from [decoder_facts] (generated from msdr's registry), so this
            script needs no msdr. ERROR lines make check() fail and the CLI exit 1; WARNING lines do not.
Means:      Python 3.11+ standard library only.
Provides:   CLI: python3 kit/check_config.py [path]; API: check(cfg: dict) -> (errors: list[str], warnings: list[str])
Ensures:    every rule under [rules] marked ERROR is enforced here; a typo under [decoders] gets the nearest decoder id.
Fails:      a file that is not valid TOML: one ERROR line with tomllib's position.
"""
from __future__ import annotations

import difflib
import os
import shutil
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent

# Where msdr looks for programs besides PATH (README: "on PATH and in ~/.local/bin"), plus the Homebrew prefixes a GUI
# launch may not have on PATH, and jt9 inside the WSJT-X app bundle.
EXTRA_DIRS = [Path.home() / ".local/bin", Path("/opt/homebrew/bin"), Path("/usr/local/bin")]
BUNDLED = {"jt9": [Path("/Applications/wsjtx.app/Contents/MacOS/jt9")]}


def khz(hz: float) -> str:
    return f"{hz / 1000:g} kHz"


def have_program(name: str) -> bool:
    if shutil.which(name):
        return True
    if any((d / name).is_file() and os.access(d / name, os.X_OK) for d in EXTRA_DIRS):
        return True
    return any(p.is_file() for p in BUNDLED.get(name, []))


def _int(cfg: dict, section: str, key: str, errors: list[str], lo: int, hi: int) -> int | None:
    v = cfg.get(section, {}).get(key)
    if not isinstance(v, int) or isinstance(v, bool) or not lo <= v <= hi:
        errors.append(f"ERROR [settings] {section}.{key} = {v!r}: needs a whole number from {lo} to {hi}")
        return None
    return v


def _port_of(listen, errors: list[str]) -> int | None:
    host, sep, port = str(listen).rpartition(":")
    if not sep or not host or not port.isdigit() or not 1 <= int(port) <= 65535:
        errors.append(f"ERROR [settings] bridge.listen = {listen!r}: needs host:port, e.g. \"127.0.0.1:5454\"")
        return None
    return int(port)


def _check_set(where: str, on: list[str], facts: dict, iq_rate, offset, errors: list[str], warnings: list[str]) -> None:
    """The rules for one set of decoders that run together in one msdr session (one profile)."""
    if not on:
        warnings.append(f"WARNING [none_on]{where} no decoder is on; the kit starts the bridge and msdr only")
        return

    iq = [n for n in on if facts[n]["path"] == "IQ"]
    audio = [n for n in on if facts[n]["path"] != "IQ"]

    # no_mix
    if iq and audio:
        errors.append(f"ERROR [no_mix]{where} raw IQ ({', '.join(iq)}) and audio ({', '.join(audio)}) are on together: "
                      "msdr runs them only separately; switch one side off")

    # one_mode
    if audio:
        common = set(facts[audio[0]]["modes"])
        for n in audio[1:]:
            common &= set(facts[n]["modes"])
        if not common:
            by_mode = ", ".join(f"{n} ({'/'.join(facts[n]['modes'])})" for n in audio)
            errors.append(f"ERROR [one_mode]{where} the audio decoders that are on share no receiver mode: {by_mode}; "
                          "keep only decoders of one mode on")
        else:
            warnings.append(f"NOTE{where} set the receiver to {'/'.join(sorted(common))} for {', '.join(audio)}")

    # iq_rate and iq_window
    if iq and iq_rate is not None:
        for n in iq:
            need = facts[n]["min_rate"]
            if iq_rate < need:
                errors.append(f"ERROR [iq_rate]{where} {n} needs iq_rate ≥ {need}, the file has {iq_rate}")
            elif offset is not None:
                reach = offset + facts[n]["width_hz"] / 2
                if reach > iq_rate / 2:
                    fit = int(2 * reach)
                    errors.append(
                        f"ERROR [iq_window]{where} {n}: VFO offset {khz(offset)} + half its width "
                        f"{khz(facts[n]['width_hz'] / 2)} = {khz(reach)} reaches past the IQ window's half width "
                        f"{khz(iq_rate / 2)}; set iq_rate ≥ {fit}")

    # programs
    for n in on:
        absent = [p for p in facts[n]["programs"] if not have_program(p)]
        if absent:
            warnings.append(f"WARNING [programs]{where} {n} needs {', '.join(absent)}, not found; msdr will skip it")


def _hostport(v) -> tuple[str, int] | None:
    host, sep, port = str(v).rpartition(":")
    if not sep or not host or not port.isdigit() or not 1 <= int(port) <= 65535:
        return None
    return host, int(port)


def resolve_profiles(cfg: dict) -> list[dict]:
    """Every msdr instance the kit starts, with defaults filled in. The first is "main": the [tci] source, the
    [bridge] and [msdr] settings and the true lines under [decoders]. Each [[profile]] adds one more instance; a
    profile without listen / web_port gets the next free port after main's (5455, 8766, ...)."""
    tci, bridge, msdr = cfg.get("tci", {}), cfg.get("bridge", {}), cfg.get("msdr", {})
    main = {
        "name": "main",
        "tci": f"{tci.get('host')}:{tci.get('port')}",
        "decoders": [n for n, v in cfg.get("decoders", {}).items() if v is True],
        "hz": "vfo",
        "iq_rate": bridge.get("iq_rate"),
        "listen": bridge.get("listen"),
        "web_port": msdr.get("port"),
    }
    out = [main]
    lp = _hostport(main["listen"])
    next_listen = (lp[1] if lp else 5454) + 1
    next_web = (main["web_port"] if isinstance(main["web_port"], int) else 8765) + 1
    for i, p in enumerate(cfg.get("profile", []) or []):
        if not isinstance(p, dict):
            out.append({"name": f"#{i + 1}", "invalid": True})
            continue
        r = dict(p)
        r.setdefault("name", f"#{i + 1}")
        r.setdefault("hz", "vfo")
        r.setdefault("iq_rate", main["iq_rate"])
        if "listen" not in r:
            r["listen"] = f"127.0.0.1:{next_listen}"
            next_listen += 1
        if "web_port" not in r:
            r["web_port"] = next_web
            next_web += 1
        out.append(r)
    return out


def _check_profiles(cfg, facts, iq_rate, offset, bridge_port, msdr_port, errors, warnings) -> None:
    """[[profile]] tables: each one more msdr instance with its own decoders, frequency and ports."""
    profiles = resolve_profiles(cfg)
    if len(profiles) == 1:
        return
    names: set[str] = set()
    ports: dict[int, str] = {}
    iq_by_source: dict[str, tuple[int, str]] = {}
    for prof in profiles:
        name = str(prof.get("name"))
        where = "" if name == "main" else f" (profile {name})"
        if prof.get("invalid"):
            errors.append(f"ERROR [profile]{where} needs to be a table: [[profile]] with name = …, tci = …, decoders = […]")
            continue
        if name in names:
            errors.append(f"ERROR [profile] two profiles are called {name!r}: give each its own name")
        names.add(name)
        src = _hostport(prof.get("tci"))
        if src is None:
            errors.append(f"ERROR [profile]{where} tci = {prof.get('tci')!r}: needs host:port of a TCI server, e.g. \"127.0.0.1:50002\"")
        lp = _hostport(prof.get("listen"))
        if lp is None:
            errors.append(f"ERROR [profile]{where} listen = {prof.get('listen')!r}: needs host:port")
        wp = prof.get("web_port")
        if not isinstance(wp, int) or isinstance(wp, bool) or not 1 <= wp <= 65535:
            errors.append(f"ERROR [profile]{where} web_port = {wp!r}: needs a port number")
            wp = None
        for port, what in ((lp[1] if lp else None, "listen"), (wp, "web_port")):
            if port is None:
                continue
            if port in ports:
                errors.append(f"ERROR [ports]{where} {what} {port} is already {ports[port]}: every bridge and every msdr needs its own port")
            else:
                ports[port] = f"{what} of {name}"
        rate = prof.get("iq_rate")
        if not isinstance(rate, int) or isinstance(rate, bool) or rate <= 0:
            errors.append(f"ERROR [profile]{where} iq_rate = {rate!r}: needs samples/s")
            rate = None
        # One TCI server makes one IQ stream (sdroxide: one DDC per radio), so every client of it gets the same rate.
        if src is not None and rate is not None:
            key = f"{src[0]}:{src[1]}"
            if key in iq_by_source and iq_by_source[key][0] != rate:
                other_rate, other = iq_by_source[key]
                errors.append(f"ERROR [shared_iq]{where} iq_rate {rate} on {key}, but {other} asks {other_rate} from the same TCI server: one server sends one IQ rate, use the same")
            iq_by_source.setdefault(key, (rate, name))
        if name == "main":
            continue  # main's decoders are checked above, from [decoders]
        decs = prof.get("decoders")
        if not isinstance(decs, list) or not decs or not all(isinstance(d, str) for d in decs):
            errors.append(f"ERROR [profile]{where} decoders = {decs!r}: needs a list of msdr decoder ids, e.g. [\"psk31\"]")
            continue
        known = []
        for d in decs:
            if d in facts:
                known.append(d)
            else:
                near = difflib.get_close_matches(d, facts, n=1)
                errors.append(f"ERROR [names]{where} {d}: no msdr decoder has this id" + (f" (did you mean {near[0]}?)" if near else ""))
        hz = prof.get("hz")
        fixed = isinstance(hz, int) and not isinstance(hz, bool) and hz > 0
        if not fixed and hz != "vfo":
            errors.append(f"ERROR [profile]{where} hz = {hz!r}: needs \"vfo\" or a frequency in Hz")
        _check_set(where, known, facts, rate, offset if hz == "vfo" else None, errors, warnings)
        if fixed and any(facts[d]["path"] == "IQ" for d in known):
            warnings.append(f"NOTE{where} hz = {hz}: whether it lies inside the IQ window is checked at the start, against the live IQ centre")


def check(cfg: dict) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []

    for section in ("tci", "bridge", "msdr", "decoders", "decoder_facts"):
        if not isinstance(cfg.get(section), dict):
            errors.append(f"ERROR [settings] section [{section}] is missing")
    if errors:
        return errors, warnings

    # Settings.
    _int(cfg, "tci", "port", errors, 1, 65535)
    offset = _int(cfg, "tci", "vfo_offset_hz", errors, 0, 100_000_000)
    iq_rate = _int(cfg, "bridge", "iq_rate", errors, 1, 100_000_000)
    msdr_port = _int(cfg, "msdr", "port", errors, 1, 65535)
    bridge_port = _port_of(cfg["bridge"].get("listen"), errors)
    for section, key in (("bridge", "spots"), ("msdr", "browser")):
        if not isinstance(cfg[section].get(key), bool):
            errors.append(f"ERROR [settings] {section}.{key} = {cfg[section].get(key)!r}: needs true or false")
    if bridge_port is not None and bridge_port == msdr_port:
        errors.append(f"ERROR [settings] bridge.listen and msdr.port both use port {bridge_port}: give each its own")

    # Names and values under [decoders].
    facts: dict = cfg["decoder_facts"]
    on: list[str] = []
    for name, value in cfg["decoders"].items():
        if name not in facts:
            near = difflib.get_close_matches(name, facts, n=1)
            hint = f" (did you mean {near[0]}?)" if near else ""
            errors.append(f"ERROR [names] {name}: no msdr decoder has this id{hint}")
        elif not isinstance(value, bool):
            errors.append(f"ERROR [names] {name} = {value!r}: needs true or false")
        elif value:
            on.append(name)
    missing = sorted(set(facts) - set(cfg["decoders"]))
    if missing:
        warnings.append(f"WARNING [names] not listed, so off: {', '.join(missing)}")

    _check_set("", on, facts, iq_rate, offset, errors, warnings)
    _check_profiles(cfg, facts, iq_rate, offset, bridge_port, msdr_port, errors, warnings)
    return errors, warnings


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "tci-kit.toml"
    try:
        cfg = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as e:
        print(f"ERROR [toml] {path}: {e}")
        return 1
    errors, warnings = check(cfg)
    for line in errors + warnings:
        print(line)
    on = [n for n, v in cfg.get("decoders", {}).items() if v is True]
    print(f"{path.name}: {len(errors)} error(s), {sum(w.startswith('WARNING') for w in warnings)} warning(s); "
          f"on: {', '.join(on) or 'none'}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
