import importlib.util
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / ".ai" / "bin" / "central_auth.py"


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
        self.runtime_state_dir = str(Path(self.tmp.name) / "state")
        self.base = {"runtime_state_dir": self.runtime_state_dir, "realm_id": "tv"}
        self.policy = {
            "roles": {
                "viewer": {"capabilities": ["tv.read"]},
                "analyst": {"capabilities": ["tv.read", "tv.chat"]},
            },
            "allowed_capabilities": ["tv.read", "tv.chat"],
        }
        auth.command_set_policy({**self.base, "policy": self.policy})

    def tearDown(self):
        self.tmp.cleanup()

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
                },
            }
        )

        checked = auth.verify_session({**self.base, "session_token": token})
        self.assertFalse(checked["valid"])
        self.assertEqual(checked["error"]["code"], "invalid_session_grants")


if __name__ == "__main__":
    unittest.main()
