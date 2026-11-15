"""Tests for kit/check_config.py against the generated kit/tci-kit.toml. Run: python3 -m unittest kit/test_check_config.py"""
import copy
import sys
import tomllib
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from check_config import check  # noqa: E402

BASE = tomllib.loads((HERE / "tci-kit.toml").read_text())


def cfg(on=(), **settings):
    c = copy.deepcopy(BASE)
    c["decoders"] = {k: k in on for k in c["decoders"]}
    for dotted, v in settings.items():
        section, key = dotted.split("__")
        c[section][key] = v
    return c


def rules(lines):
    return {l.split("[", 1)[1].split("]", 1)[0] for l in lines}


class Check(unittest.TestCase):
    def test_generated_file_is_clean(self):
        errors, _ = check(BASE)
        self.assertEqual(errors, [])

    def test_iq_and_audio_together(self):
        errors, _ = check(cfg(["rtl433", "psk31"]))
        self.assertIn("no_mix", rules(errors))

    def test_audio_without_common_mode(self):
        errors, _ = check(cfg(["psk31", "pocsag"]))
        self.assertIn("one_mode", rules(errors))

    def test_usb_and_usb_lsb_share_usb(self):
        errors, notes = check(cfg(["psk31", "packet300", "sstv"]))
        self.assertEqual(errors, [])
        self.assertTrue(any("receiver to USB" in n for n in notes))

    def test_rtl433_at_250k_misses_the_window(self):
        # Measured 2026-11-14: msdr refused rtl_433 at 250 kS/s behind SDR Oxide (VFO 28 kHz off centre).
        errors, _ = check(cfg(["rtl433"], bridge__iq_rate=250000))
        self.assertIn("iq_window", rules(errors))
        self.assertTrue(any("≥ 256000" in e for e in errors))

    def test_adsb_needs_its_rate(self):
        errors, _ = check(cfg(["adsb"]))
        self.assertIn("iq_rate", rules(errors))

    def test_typo_gets_a_suggestion(self):
        c = cfg(["rtl433"])
        c["decoders"]["rtl_433"] = True
        errors, _ = check(c)
        self.assertTrue(any("did you mean rtl433" in e for e in errors))

    def test_value_must_be_bool(self):
        c = cfg()
        c["decoders"]["rtl433"] = "yes"
        errors, _ = check(c)
        self.assertIn("names", rules(errors))

    def test_ports_must_differ(self):
        errors, _ = check(cfg(["rtl433"], msdr__port=5454))
        self.assertIn("settings", rules(errors))

    def test_nothing_on_is_only_a_warning(self):
        errors, warnings = check(cfg())
        self.assertEqual(errors, [])
        self.assertIn("none_on", rules(warnings))

    def test_profiles_run_beside_main(self):
        c = cfg(["rtl433"])
        c["profile"] = [{"name": "psk20m", "tci": "127.0.0.1:50002", "decoders": ["psk31"], "hz": 14070000}]
        errors, notes = check(c)
        self.assertEqual(errors, [])
        self.assertTrue(any("(profile psk20m) set the receiver to USB" in n for n in notes))

    def test_profile_rules_apply_per_profile(self):
        c = cfg(["rtl433"])
        c["profile"] = [{"name": "mixed", "tci": "127.0.0.1:50002", "decoders": ["rtl433", "psk31"]}]
        errors, _ = check(c)
        self.assertTrue(any(e.startswith("ERROR [no_mix] (profile mixed)") for e in errors))

    def test_profile_ports_must_differ(self):
        c = cfg(["rtl433"])
        c["profile"] = [{"name": "a", "tci": "127.0.0.1:50002", "decoders": ["psk31"], "web_port": 8765}]
        errors, _ = check(c)
        self.assertIn("ports", rules(errors))

    def test_one_tci_server_one_iq_rate(self):
        c = cfg(["rtl433"])
        c["profile"] = [{"name": "ft8", "tci": "127.0.0.1:50001", "decoders": ["ft8"], "iq_rate": 250000}]
        errors, _ = check(c)
        self.assertIn("shared_iq", rules(errors))

    def test_profile_typo_and_bad_hz(self):
        c = cfg(["rtl433"])
        c["profile"] = [{"name": "x", "tci": "127.0.0.1:50002", "decoders": ["psk32"], "hz": "auto"}]
        errors, _ = check(c)
        self.assertTrue(any("did you mean psk31" in e for e in errors))
        self.assertTrue(any("hz = 'auto'" in e for e in errors))


if __name__ == "__main__":
    unittest.main()
