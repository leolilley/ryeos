import hashlib
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = (
    Path(__file__).resolve().parents[1]
    / ".ai"
    / "tools"
    / "central-auth"
    / "lib"
    / "central_auth.py"
)


def load_module():
    spec = importlib.util.spec_from_file_location("central_auth", MODULE_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    module.PBKDF2_ITERATIONS = 10_000
    return module


auth = load_module()


class CentralAuthTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.app_root = Path(self.tmp.name) / "app-root"
        self.realm_dir = self.app_root / ".ai" / "state" / "bundle-state" / "central-auth" / "tv"
        self.env = mock.patch.dict(
            os.environ,
            {"RYEOSD_BUNDLE_STATE_HOME": str(self.realm_dir), "RYEOS_APP_ROOT": str(self.app_root)},
        )
        self.env.start()
        self.base = {"realm_id": "tv"}
        self.throttle = {"max_failures": 3, "window_secs": 600, "lockout_secs": 300}
        self.policy = {
            "roles": {
                "viewer": {"capabilities": ["tv.read"]},
                "analyst": {"capabilities": ["tv.read", "tv.chat"]},
            },
            "allowed_capabilities": ["tv.read", "tv.chat"],
            "login_throttle": self.throttle,
        }
        auth.command_set_policy({**self.base, "policy": self.policy})

    def tearDown(self):
        self.env.stop()
        self.tmp.cleanup()

    def login(self, passphrase, principal_id="alice", **extra):
        return auth.command_login(
            {
                **self.base,
                "method": "passphrase",
                "principal_id": principal_id,
                "passphrase": passphrase,
                **extra,
            }
        )

    def create_alice(self, passphrase=" secret123 "):
        return auth.command_create_principal(
            {
                **self.base,
                "principal_id": "alice",
                "display_name": "Alice",
                "roles": ["analyst"],
                "passphrase": passphrase,
                "bootstrap": True,
            }
        )

    def test_passphrase_login_preserves_whitespace_and_revoke_invalidates_session(self):
        self.create_alice()

        bad = auth.command_login(
            {
                **self.base,
                "method": "passphrase",
                "principal_id": "alice",
                "passphrase": "secret123",
            }
        )
        self.assertFalse(bad["ok"])
        self.assertEqual(bad["error"]["code"], "invalid_credentials")

        login = auth.command_login(
            {
                **self.base,
                "method": "passphrase",
                "principal_id": "alice",
                "passphrase": " secret123 ",
            }
        )
        self.assertTrue(login["ok"])
        token = login["session_token"]

        checked = auth.command_check_capability(
            {**self.base, "session_token": token, "required_capability": "tv.chat"}
        )
        self.assertTrue(checked["valid"])
        self.assertTrue(checked["allowed"])

        revoked = auth.command_revoke_session({**self.base, "session_token": token})
        self.assertTrue(revoked["revoked"])

        after_revoke = auth.verify_session({**self.base, "session_token": token})
        self.assertFalse(after_revoke["valid"])
        self.assertEqual(after_revoke["error"]["code"], "invalid_session")

    def test_check_capability_requires_required_capability(self):
        self.create_alice()
        login = auth.command_login(
            {
                **self.base,
                "method": "passphrase",
                "principal_id": "alice",
                "passphrase": " secret123 ",
            }
        )
        with self.assertRaises(auth.AuthError) as ctx:
            auth.command_check_capability({**self.base, "session_token": login["session_token"]})
        self.assertEqual(ctx.exception.code, "invalid_request")

    def test_invite_redemption_is_one_use(self):
        invite = auth.command_create_invite({**self.base, "roles": ["viewer"], "ttl_secs": 3600})
        code = invite["invite_code"]

        login = auth.command_login(
            {
                **self.base,
                "method": "invite",
                "invite_code": code,
                "principal_id": "bob",
                "display_name": "Bob",
                "passphrase": "secret123",
            }
        )
        self.assertTrue(login["ok"])
        self.assertEqual(login["principal"]["roles"], ["viewer"])

        reused = auth.command_login(
            {
                **self.base,
                "method": "invite",
                "invite_code": code,
                "principal_id": "charlie",
                "display_name": "Charlie",
                "passphrase": "secret123",
            }
        )
        self.assertFalse(reused["ok"])
        self.assertEqual(reused["error"]["code"], "invalid_credentials")

    def test_bad_integer_input_is_invalid_request(self):
        with self.assertRaises(auth.AuthError) as ctx:
            auth.command_create_invite({**self.base, "roles": ["viewer"], "ttl_secs": "oops"})
        self.assertEqual(ctx.exception.code, "invalid_request")

    def test_policy_drift_invalidates_session_cleanly(self):
        self.create_alice()
        login = auth.command_login(
            {
                **self.base,
                "method": "passphrase",
                "principal_id": "alice",
                "passphrase": " secret123 ",
            }
        )
        token = login["session_token"]

        auth.command_set_policy(
            {
                **self.base,
                "policy": {
                    "roles": {"viewer": {"capabilities": ["tv.read"]}},
                    "allowed_capabilities": ["tv.read"],
                    "login_throttle": self.throttle,
                },
            }
        )

        checked = auth.verify_session({**self.base, "session_token": token})
        self.assertFalse(checked["valid"])
        self.assertEqual(checked["error"]["code"], "invalid_session_grants")

    def test_state_home_from_env(self):
        self.create_alice()
        self.assertTrue((self.realm_dir / "principals" / "alice.json").is_file())
        self.assertTrue((self.realm_dir / "policy.json").is_file())
        self.assertFalse((self.realm_dir / "realms").exists())

    def test_runtime_state_dir_is_refused(self):
        for key in ("runtime_state_dir", "project_path"):
            with self.subTest(key=key):
                with self.assertRaises(auth.AuthError) as ctx:
                    auth.command_gc({**self.base, key: str(Path(self.tmp.name) / "elsewhere")})
                self.assertEqual(ctx.exception.code, "caller_state_location_refused")
        self.assertFalse((Path(self.tmp.name) / "elsewhere").exists())

    def test_missing_state_home_refused(self):
        for value in (None, "relative/home"):
            with self.subTest(value=value):
                env = {k: v for k, v in os.environ.items() if k != "RYEOSD_BUNDLE_STATE_HOME"}
                if value is not None:
                    env["RYEOSD_BUNDLE_STATE_HOME"] = value
                with mock.patch.dict(os.environ, env, clear=True):
                    with self.assertRaises(auth.AuthError) as ctx:
                        auth.command_gc(self.base)
                self.assertEqual(ctx.exception.code, "state_home_unavailable")
                if value is None:
                    self.assertIn("state_home declaration", ctx.exception.message)

    def test_app_root_env_ignored(self):
        env = {k: v for k, v in os.environ.items() if k != "RYEOSD_BUNDLE_STATE_HOME"}
        with mock.patch.dict(os.environ, env, clear=True):
            with self.assertRaises(auth.AuthError) as ctx:
                auth.command_gc(self.base)
        self.assertEqual(ctx.exception.code, "state_home_unavailable")
        self.assertFalse((self.app_root / ".ai" / "state" / "central-auth").exists())

    def test_realm_must_match_mounted_namespace(self):
        with self.assertRaises(auth.AuthError) as ctx:
            auth.command_gc({"realm_id": "other"})
        self.assertEqual(ctx.exception.code, "state_home_unavailable")

    def test_enforced_mount_is_trusted(self):
        with mock.patch.dict(os.environ, {"RYEOSD_BUNDLE_STATE_HOME": auth.ENFORCED_STATE_MOUNT}):
            self.assertEqual(
                auth.resolve_realm_dir({"realm_id": "other"}), Path(auth.ENFORCED_STATE_MOUNT)
            )

    def test_login_missing_principal_runs_kdf(self):
        for principal_id, disabled in (("nobody", False), ("alice", True)):
            with self.subTest(principal_id=principal_id):
                if disabled:
                    auth.command_create_principal(
                        {
                            **self.base,
                            "principal_id": "alice",
                            "display_name": "Alice",
                            "roles": ["analyst"],
                            "passphrase": "secret123",
                            "disabled": True,
                        }
                    )
                with mock.patch.object(
                    auth.hashlib, "pbkdf2_hmac", wraps=hashlib.pbkdf2_hmac
                ) as kdf:
                    result = self.login("secret123", principal_id=principal_id)
                self.assertEqual(result["error"]["code"], "invalid_credentials")
                self.assertEqual(kdf.call_count, 1)

    def test_login_locks_after_max_failures(self):
        self.create_alice()
        for _ in range(self.throttle["max_failures"]):
            self.assertEqual(self.login("wrong-pass")["error"]["code"], "invalid_credentials")
        locked = self.login(" secret123 ")
        self.assertFalse(locked["ok"])
        self.assertEqual(locked["error"]["code"], "locked")
        self.assertGreater(locked["error"]["retry_after_secs"], 0)
        # Unknown principals lock the same way, so a lockout reveals nothing.
        for _ in range(self.throttle["max_failures"]):
            self.login("wrong-pass", principal_id="nobody")
        self.assertEqual(self.login("wrong-pass", principal_id="nobody")["error"]["code"], "locked")

    def test_client_address_is_throttled_across_principals(self):
        self.create_alice()
        for i in range(self.throttle["max_failures"]):
            self.login("wrong-pass", principal_id=f"guess{i}", client_address="203.0.113.7")
        locked = self.login(" secret123 ", client_address="203.0.113.7")
        self.assertEqual(locked["error"]["code"], "locked")
        self.assertTrue(self.login(" secret123 ", client_address="198.51.100.2")["ok"])

    def test_lockout_expires(self):
        self.create_alice()
        start = auth.now_unix()
        with mock.patch.object(auth, "now_unix", return_value=start):
            for _ in range(self.throttle["max_failures"]):
                self.login("wrong-pass")
            self.assertEqual(self.login(" secret123 ")["error"]["code"], "locked")
        with mock.patch.object(
            auth, "now_unix", return_value=start + self.throttle["lockout_secs"]
        ):
            self.assertTrue(self.login(" secret123 ")["ok"])

    def test_success_resets_failures(self):
        self.create_alice()
        for _ in range(self.throttle["max_failures"] - 1):
            self.login("wrong-pass")
        self.assertTrue(self.login(" secret123 ")["ok"])
        for _ in range(self.throttle["max_failures"] - 1):
            self.login("wrong-pass")
        self.assertTrue(self.login(" secret123 ")["ok"])

    def test_policy_without_login_throttle_is_refused(self):
        policy = {k: v for k, v in self.policy.items() if k != "login_throttle"}
        with self.assertRaises(auth.AuthError) as ctx:
            auth.command_set_policy({**self.base, "policy": policy})
        self.assertEqual(ctx.exception.code, "policy_missing_login_throttle")
        # A realm stored before login_throttle existed is refused at load.
        (self.realm_dir / "policy.json").write_text(json.dumps(policy))
        with self.assertRaises(auth.AuthError) as ctx:
            self.login("secret123")
        self.assertEqual(ctx.exception.code, "policy_missing_login_throttle")

    def test_expired_sessions_purged_on_login(self):
        self.create_alice()
        first = self.login(" secret123 ", ttl_secs=60)
        stale = auth.session_path(self.realm_dir, first["session_token"])
        self.assertTrue(stale.exists())
        with mock.patch.object(auth, "now_unix", return_value=auth.now_unix() + 3600):
            self.assertTrue(self.login(" secret123 ")["ok"])
        self.assertFalse(stale.exists())

    def test_expired_session_removed_on_verify(self):
        self.create_alice()
        token = self.login(" secret123 ", ttl_secs=60)["session_token"]
        path = auth.session_path(self.realm_dir, token)
        with mock.patch.object(auth, "now_unix", return_value=auth.now_unix() + 3600):
            checked = auth.verify_session({**self.base, "session_token": token})
        self.assertFalse(checked["valid"])
        self.assertFalse(path.exists())

    def test_gc_removes_stale_failure_records(self):
        self.create_alice()
        self.login("wrong-pass")
        self.assertEqual(len(list((self.realm_dir / "failures").glob("*.json"))), 1)
        with mock.patch.object(
            auth, "now_unix", return_value=auth.now_unix() + self.throttle["window_secs"]
        ):
            result = auth.command_gc(self.base)
        self.assertEqual(result["removed_failures"], 1)


if __name__ == "__main__":
    unittest.main()
