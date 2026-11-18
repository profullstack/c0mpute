"""Unit tests: no real DNS or systemd state is changed."""
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MODULE = Path(__file__).resolve().parents[1] / "scripts" / "dev2-dns-recovery.py"
spec = importlib.util.spec_from_file_location("dev2_dns_recovery", MODULE)
dns = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dns)

CLICONF = "# Written by `moshcode dns enable`.\n[Resolve]\nDNS=127.0.0.1:5354\nDomains=~.\n"
PROXYCONF = "[Resolve]\nDNS=127.0.0.1:5354\nDomains=~.\n"
UNRELATED = "[Resolve]\nDNS=1.1.1.1\nDomains=~.\n"

class RecoveryTests(unittest.TestCase):
    def test_detects_owned_dropins_not_foreign_configs(self):
        self.assertTrue(dns.owned("moshpit.conf", CLICONF))
        self.assertTrue(dns.owned("00-moshpit.conf", PROXYCONF))
        self.assertFalse(dns.owned("00-moshpit.conf", UNRELATED))
        self.assertFalse(dns.owned("corp.conf", CLICONF))
        self.assertFalse(dns.owned("moshpit.conf", UNRELATED))

    def test_filters_unrelated_and_refuses_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "moshpit.conf").write_text(CLICONF)
            (d / "00-moshpit.conf").write_text(UNRELATED)
            self.assertEqual(dns.candidates(d), [d / "moshpit.conf"])
            (d / "moshpit.conf").unlink()
            (d / "moshpit.conf").symlink_to(d / "00-moshpit.conf")
            with self.assertRaises(RuntimeError):
                dns.candidates(d)

    def test_apply_preserves_other_dns_configuration(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "etc"
            backup = root / "backup"
            config.mkdir()
            (config / "moshpit.conf").write_text(CLICONF)
            (config / "corp.conf").write_text(UNRELATED)
            with mock.patch.object(dns, "run"), mock.patch.object(dns, "dns_works", return_value=True):
                self.assertEqual(dns.apply(config, backup), 0)
            self.assertFalse((config / "moshpit.conf").exists())
            self.assertEqual((config / "corp.conf").read_text(), UNRELATED)
            self.assertEqual(len(list(backup.glob("*/moshpit.conf"))), 1)

    def test_failed_verification_restores_originals(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "etc"
            backup = root / "backup"
            config.mkdir()
            (config / "moshpit.conf").write_text(CLICONF)
            (config / "00-moshpit.conf").write_text(PROXYCONF)
            with mock.patch.object(dns, "run") as run, mock.patch.object(dns, "dns_works", return_value=False):
                self.assertEqual(dns.apply(config, backup), 1)
            self.assertEqual((config / "moshpit.conf").read_text(), CLICONF)
            self.assertEqual((config / "00-moshpit.conf").read_text(), PROXYCONF)
            self.assertEqual(run.call_count, 2)

    def test_restart_error_rolls_back(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "etc"
            backup = root / "backup"
            config.mkdir()
            (config / "moshpit.conf").write_text(CLICONF)
            with mock.patch.object(dns, "run", side_effect=[OSError("systemd offline"), None]):
                self.assertEqual(dns.apply(config, backup), 1)
            self.assertEqual((config / "moshpit.conf").read_text(), CLICONF)

if __name__ == "__main__":
    unittest.main()
