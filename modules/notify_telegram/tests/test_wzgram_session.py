"""Pure-stdlib regression tests for the WZGram session bridge.

These tests do not require the WZGram virtualenv; they only exercise the
session-address conversion and WZ v3 session-string encoding in the sidecar.
"""

import importlib.util
import sqlite3
import struct
import tempfile
import unittest
import zlib
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parents[1] / "wzgram_upload.py"
_SPEC = importlib.util.spec_from_file_location("wzgram_upload", MODULE_PATH)
wz = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(wz)


def _decode_session_string(value: str) -> bytes:
    body = value[3:] if value.startswith("WZ_") else value
    out = bytearray()
    for i in range(0, len(body), 4):
        chunk = body[i:i + 4]
        chars = [wz._ALPHABET.index(c) for c in chunk]
        if len(chunk) == 4:
            n = (chars[0] << 18) | (chars[1] << 12) | (chars[2] << 6) | chars[3]
            out.extend(((n >> 16) & 255, (n >> 8) & 255, n & 255))
        elif len(chunk) == 3:
            n = (chars[0] << 12) | (chars[1] << 6) | chars[2]
            out.extend(((n >> 10) & 255, (n >> 2) & 255))
        elif len(chunk) == 2:
            n = (chars[0] << 6) | chars[1]
            out.append((n >> 2) & 255)
    return bytes(out)


class SplitDcAddressTests(unittest.TestCase):
    def test_grammers_ipv4_socket_address(self):
        self.assertEqual(wz._split_dc_address("149.154.167.41:443"), ("149.154.167.41", 443))

    def test_grammers_ipv6_socket_address(self):
        self.assertEqual(
            wz._split_dc_address("[2001:db8::1]:443"),
            ("2001:db8::1", 443),
        )

    def test_bare_ip_uses_443(self):
        self.assertEqual(wz._split_dc_address("149.154.175.100"), ("149.154.175.100", 443))

    def test_custom_port_is_preserved(self):
        self.assertEqual(wz._split_dc_address("1.2.3.4:8443"), ("1.2.3.4", 8443))

    def test_invalid_port_falls_back_to_443(self):
        self.assertEqual(wz._split_dc_address("1.2.3.4:70000"), ("1.2.3.4", 443))


class SessionStringTests(unittest.TestCase):
    def test_session_string_uses_bare_host_and_separate_port(self):
        auth_key = bytes(range(256))
        value = wz._session_string(2, 12345, auth_key, "149.154.167.41", 443)
        raw = _decode_session_string(value)
        payload, checksum = raw[:-4], raw[-4:]
        self.assertEqual(struct.unpack("<I", checksum)[0], zlib.crc32(payload))

        fields = struct.unpack(">BBI?256sQ?H48s", payload)
        self.assertEqual(fields[0], 3)
        self.assertEqual(fields[1], 2)
        self.assertEqual(fields[2], 12345)
        self.assertEqual(fields[4], auth_key)
        self.assertEqual(fields[7], 443)
        self.assertEqual(fields[8].split(b"\0", 1)[0], b"149.154.167.41")
        self.assertNotIn(b":", fields[8].split(b"\0", 1)[0])


class GrammersSessionTests(unittest.TestCase):
    def test_reads_normalized_host_from_grammers_sqlite(self):
        auth_key = bytes(range(256))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.db"
            conn = sqlite3.connect(path)
            conn.execute("CREATE TABLE dc_home (dc_id INTEGER NOT NULL)")
            conn.execute(
                "CREATE TABLE dc_option ("
                "dc_id INTEGER NOT NULL, ipv4 TEXT NOT NULL, "
                "ipv6 TEXT NOT NULL, auth_key BLOB)"
            )
            conn.execute("INSERT INTO dc_home VALUES (2)")
            conn.execute(
                "INSERT INTO dc_option VALUES (2, ?, ?, ?)",
                ("149.154.167.41:443", "", auth_key),
            )
            conn.commit()
            conn.close()

            self.assertEqual(
                wz._read_grammers_session(str(path)),
                (2, auth_key, "149.154.167.41", 443),
            )


if __name__ == "__main__":
    unittest.main()
