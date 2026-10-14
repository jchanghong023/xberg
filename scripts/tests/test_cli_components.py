"""Exercise the real component packager on tiny staged bytes, without building Xberg.

Run from the repository root: python scripts/tests/test_cli_components.py
Requires PowerShell >= 7.4; temporary inputs and outputs stay under .tmp/.
"""

import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile


REPO_ROOT = Path(__file__).resolve().parents[2]
PACKAGER = REPO_ROOT / "scripts/publish/cli/package-cli-components.ps1"
TARGET = "x86_64-pc-windows-msvc"
STAGE_NAME = f"xberg-cli-{TARGET}"
MANIFEST_NAME = f"xberg-components-{TARGET}.json"


class CliComponentPackagingTests(unittest.TestCase):
    def setUp(self):
        self.pwsh = shutil.which("pwsh")
        if self.pwsh is None:
            self.fail("PowerShell >= 7.4 (pwsh) is required")
        temp_root = REPO_ROOT / ".tmp"
        temp_root.mkdir(exist_ok=True)
        self.workspace = tempfile.TemporaryDirectory(prefix="cli-components-", dir=temp_root)
        self.addCleanup(self.workspace.cleanup)
        self.root = Path(self.workspace.name)
        self.stage = self.root / STAGE_NAME
        self.output = self.root / "assets"
        self.groups = {
            "common": {
                "xberg.exe": b"synthetic executable, not runnable",
                "xberg.cmd": b"synthetic launcher",
                "onnxruntime.dll": b"shared runtime",
                "onnxruntime_providers_shared.dll": b"shared providers",
                "vcruntime140.dll": b"shared CRT",
                "LICENSE": b"project license",
                "THIRD_PARTY_LICENSES.md": b"third party notices",
                "models/paddleocr-onnx-models-LICENSE.txt": b"shared OCR license",
                "support/.hidden": b"support bytes",
                "support/empty.txt": b"",
                "support/\u8bf4\u660e.txt": "Unicode support".encode(),
            },
            "document": {
                "models/models--xberg-io--paddleocr-onnx-models/snapshots/abc/v6/det/tiny/model.onnx": b"document det",
                "models/models--xberg-io--paddleocr-onnx-models/snapshots/abc/v6/rec/tiny/dict.txt": b"document dictionary",
            },
            "snapshot": {
                "models/snapshot-ocr/det.onnx": b"snapshot det",
                "models/snapshot-ocr/rec.onnx": b"snapshot rec",
                "models/snapshot-ocr/dict/dict.txt": b"snapshot dictionary",
            },
            "media": {
                "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx": b"sensevoice",
                "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt": b"tokens",
                "models/vad/silero_vad.onnx": b"vad",
                "ffmpeg/avcodec-63.dll": b"ffmpeg codec",
                "ffmpeg/LICENSE.txt": b"ffmpeg license",
                "sherpa-onnx/sherpa-onnx-c-api.dll": b"sherpa API",
                "sherpa-onnx/onnxruntime.dll": b"shared runtime",
                "sherpa-onnx/LICENSE": b"sherpa license",
                "samples/silence-1s.wav": b"audio fixture",
            },
        }
        for files in self.groups.values():
            for relative, content in files.items():
                path = self.stage / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)

    def package(self, stage=None, output=None):
        return subprocess.run(
            [self.pwsh, "-NoProfile", "-NonInteractive", "-File", str(PACKAGER),
             "-Stage", str(stage or self.stage),
             "-OutputDirectory", str(output or self.output)],
            capture_output=True, text=True, encoding="utf-8", errors="replace", check=False,
        )

    def test_independent_archives_hashes_licenses_and_lossless_recombination(self):
        result = self.package()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        manifest = json.loads((self.output / MANIFEST_NAME).read_text(encoding="utf-8"))
        self.assertEqual(manifest["schema_version"], 1)
        self.assertEqual(manifest["target"], TARGET)
        self.assertEqual([c["id"] for c in manifest["components"]], ["document", "snapshot", "media"])
        self.assertEqual(
            {p.name for p in self.output.iterdir()},
            {MANIFEST_NAME, *(f"xberg-{name}-{TARGET}.zip" for name in ("document", "snapshot", "media"))},
        )
        combined = {}
        common_metadata = {}
        for component in manifest["components"]:
            feature = component["id"]
            self.assertEqual(component["requires"], [])
            self.assertEqual(component["archive"], f"xberg-{feature}-{TARGET}.zip")
            archive_path = self.output / component["archive"]
            archive_bytes = archive_path.read_bytes()
            self.assertEqual(component["size_bytes"], len(archive_bytes))
            self.assertEqual(component["sha256"], hashlib.sha256(archive_bytes).hexdigest())
            members = component["members"]
            paths = [member["path"] for member in members]
            expected = self.groups["common"] | self.groups[feature]
            self.assertEqual(len(paths), len(set(paths)), "duplicate archive ownership")
            self.assertEqual(set(paths), set(expected))
            with zipfile.ZipFile(archive_path) as archive:
                self.assertEqual(set(archive.namelist()), {f"{STAGE_NAME}/{path}" for path in expected})
                for member in members:
                    relative = member["path"]
                    content = archive.read(f"{STAGE_NAME}/{relative}")
                    self.assertEqual(content, expected[relative])
                    self.assertEqual(member["size_bytes"], len(content))
                    self.assertEqual(member["sha256"], hashlib.sha256(content).hexdigest())
                    if relative in self.groups["common"]:
                        prior = common_metadata.setdefault(relative, member)
                        self.assertEqual(prior, member, "shared metadata must be byte-identical")
                    if relative in combined:
                        self.assertEqual(combined[relative], content, "conflicting overlap")
                    combined[relative] = content
        staged = {p.relative_to(self.stage).as_posix(): p.read_bytes() for p in self.stage.rglob("*") if p.is_file()}
        self.assertEqual(combined, staged)

    def test_unknown_model_fails_without_leaving_stale_manifest(self):
        self.output.mkdir()
        manifest_path = self.output / MANIFEST_NAME
        manifest_path.write_text("stale manifest", encoding="utf-8")
        unknown = self.stage / "models/unassigned/new-model.onnx"
        unknown.parent.mkdir()
        unknown.write_bytes(b"not classified")
        result = self.package()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No component ownership rule", result.stderr)
        self.assertFalse(manifest_path.exists())

    def test_missing_feature_fails_instead_of_advertising_common_only_archive(self):
        shutil.rmtree(self.stage / "models/snapshot-ocr")
        result = self.package()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Component 'snapshot' has no staged files", result.stderr)
        self.assertFalse((self.output / MANIFEST_NAME).exists())

    def test_missing_component_license_fails(self):
        (self.stage / "ffmpeg/LICENSE.txt").unlink()
        result = self.package()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Required staged runtime/license file missing", result.stderr)
        self.assertFalse((self.output / MANIFEST_NAME).exists())

    def test_archive_source_root_is_not_silently_renamed(self):
        wrong_stage = self.stage.rename(self.root / "wrong-root")
        result = self.package(stage=wrong_stage)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Stage must be a directory named", result.stderr)

    def test_output_cannot_be_part_of_the_source_tree(self):
        result = self.package(output=self.stage / "archives")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("OutputDirectory must be outside Stage", result.stderr)
        self.assertFalse((self.stage / "archives").exists())


if __name__ == "__main__":
    unittest.main()
