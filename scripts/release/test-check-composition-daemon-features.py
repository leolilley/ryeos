#!/usr/bin/env python3
"""Self-test for check-composition-daemon-features.py: a daemon built without a
feature its bundle set needs is refused, a matching one passes."""
import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("check", ROOT / "scripts/release/check-composition-daemon-features.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

PAIR = """FROM composition-publication AS {pair}
RUN source scripts/pkg/bundle-sets.sh && for package in ryeosd ryeos-cli; do \\
  selected="$(ryeos_bundle_set_cargo_features {features_set} "$package")"; done && \\
  cargo build --release --locked --no-default-features -p ryeosd -p ryeos-cli
FROM debian AS ryeos-image
COPY --from={pair} /build/target/x/ryeosd /usr/local/bin/ryeosd
ENV RYEOS_INIT_NODE_PROFILE=hosted-workflow
"""
DIRS = {("directive", "model-providers"), ("scheduling", "scheduler")}


class CheckTest(unittest.TestCase):
    def run_check(self, text):
        return check.check(ROOT, text, DIRS)[0]

    def test_matching_pair_passes(self):
        self.assertEqual(self.run_check(PAIR.format(pair="p", features_set="hosted-workflow")), [])

    def test_pair_without_provider_feature_is_refused(self):
        problems = self.run_check(PAIR.format(pair="p", features_set="core-scheduling"))
        self.assertEqual(len(problems), 1)
        self.assertIn("model-provider-setup", problems[0])

    def test_featureless_daemon_is_refused(self):
        text = PAIR.format(pair="p", features_set="core").replace("COPY --from=p", "COPY --from=scheduler-free-publication")
        problems = self.run_check(text)
        self.assertIn("scheduling", problems[0])

    def test_publication_daemon_carries_the_release_artifacts_features(self):
        text = PAIR.format(pair="p", features_set="core").replace("COPY --from=p", "COPY --from=composition-publication")
        self.assertEqual(self.run_check(text), [])

    def test_repository_dockerfile_and_ownership_agree(self):
        problems, checked = check.check(ROOT, (ROOT / "Dockerfile.release").read_text(), check.tracked_service_dirs(ROOT))
        self.assertEqual(problems + check.ownership_agrees(ROOT), [])
        self.assertTrue(any(name == "ryeos-hosted-workflow" for name, *_ in checked))


if __name__ == "__main__":
    unittest.main()
