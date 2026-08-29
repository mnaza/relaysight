"""The storage plugin's key builder.

`object_key` is the only place where client-supplied strings become a path
inside the storage bucket, so a traversal here writes to, or reads from,
another tenant's prefix. It is pure and cheap to test; the rest of the plugin
is HTTP glue around boto3.

boto3 is stubbed rather than installed: the module builds S3 clients at import
time, and requiring the real SDK in CI to test twenty lines of string handling
is a poor trade. The function under test is the real one from the real file.
"""

import importlib.util
import sys
import types
import unittest
from pathlib import Path

PLUGIN = Path(__file__).resolve().parents[1] / "storage-s3" / "plugin.py"


def load_plugin(prefix=""):
    """Import plugin.py with boto3 stubbed and S3_PREFIX set."""
    boto3 = types.ModuleType("boto3")
    boto3.client = lambda *args, **kwargs: object()
    botocore = types.ModuleType("botocore")
    botocore_config = types.ModuleType("botocore.config")
    botocore_config.Config = lambda *args, **kwargs: None
    saved = {name: sys.modules.get(name) for name in ("boto3", "botocore", "botocore.config")}
    sys.modules.update({"boto3": boto3, "botocore": botocore, "botocore.config": botocore_config})

    import os
    previous_prefix = os.environ.get("S3_PREFIX")
    os.environ["S3_PREFIX"] = prefix
    try:
        spec = importlib.util.spec_from_file_location("storage_s3_plugin", PLUGIN)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module
    finally:
        if previous_prefix is None:
            os.environ.pop("S3_PREFIX", None)
        else:
            os.environ["S3_PREFIX"] = previous_prefix
        for name, value in saved.items():
            if value is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = value


class ObjectKeyTest(unittest.TestCase):
    def setUp(self):
        self.plugin = load_plugin()

    def test_joins_namespace_and_key(self):
        self.assertEqual(self.plugin.object_key("recordings", "cam-1/seg-0.m4s"),
                         "recordings/cam-1/seg-0.m4s")

    def test_strips_stray_separators(self):
        # Callers assemble these from ids that may or may not carry slashes;
        # a doubled separator makes two different keys for the same object.
        self.assertEqual(self.plugin.object_key("/recordings/", "/cam-1"), "recordings/cam-1")

    def test_skips_empty_segments(self):
        self.assertEqual(self.plugin.object_key("", "cam-1"), "cam-1")

    def test_rejects_traversal_in_the_key(self):
        # Writing outside the namespace means writing into another tenant's.
        with self.assertRaises(ValueError):
            self.plugin.object_key("recordings", "../secrets/key")

    def test_rejects_traversal_in_the_namespace(self):
        with self.assertRaises(ValueError):
            self.plugin.object_key("../..", "cam-1")

    def test_rejects_traversal_buried_mid_path(self):
        with self.assertRaises(ValueError):
            self.plugin.object_key("recordings", "cam-1/../../etc/passwd")

    def test_allows_dots_that_are_not_traversal(self):
        # A refusal that catches ordinary filenames would be its own bug.
        self.assertEqual(self.plugin.object_key("recordings", "clip..mp4"),
                         "recordings/clip..mp4")
        self.assertEqual(self.plugin.object_key("recordings", "...hidden"),
                         "recordings/...hidden")

    def test_a_configured_prefix_cannot_be_escaped(self):
        plugin = load_plugin(prefix="tenant-a")
        self.assertEqual(plugin.object_key("recordings", "cam-1"), "tenant-a/recordings/cam-1")
        with self.assertRaises(ValueError):
            plugin.object_key("recordings", "../../tenant-b/cam-1")


class SignerAudienceTest(unittest.TestCase):
    def setUp(self):
        self.plugin = load_plugin()

    def test_known_audiences_resolve(self):
        for audience in ("browser", "edge", "service"):
            _, resolved = self.plugin.signer_for({"audience": audience})
            self.assertEqual(resolved, audience)

    def test_audience_defaults_to_service(self):
        # The narrowest one: an omitted audience must not hand out a URL the
        # public internet can reach.
        _, resolved = self.plugin.signer_for({})
        self.assertEqual(resolved, "service")

    def test_unknown_audience_is_refused(self):
        with self.assertRaises(ValueError):
            self.plugin.signer_for({"audience": "public"})


if __name__ == "__main__":
    unittest.main()
