import pathlib
import shutil
import subprocess
import tempfile
import unittest


class ReleasePlanTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        (self.root / "script").mkdir()
        shutil.copy2(
            pathlib.Path(__file__).with_name("release-plan"), self.root / "script"
        )
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
        self.git("init", "--quiet")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release@example.invalid")
        self.commit("Package version")

    def git(self, *arguments):
        return subprocess.run(
            ["git", *arguments],
            cwd=self.root,
            check=True,
            capture_output=True,
            text=True,
        )

    def commit(self, message):
        self.git("add", "Cargo.toml")
        self.git("commit", "--quiet", "-m", message)

    def plan(self, branch="main", run="7", event="push", succeeds=True):
        result = subprocess.run(
            [str(self.root / "script/release-plan"), branch, run, event],
            cwd=self.root,
            check=False,
            capture_output=True,
            text=True,
        )
        if not succeeds:
            self.assertNotEqual(result.returncode, 0)
            return
        self.assertEqual(result.returncode, 0, result.stderr)
        return dict(line.split("=", 1) for line in result.stdout.splitlines())

    def test_beta_and_production_tags(self):
        self.assertEqual(self.plan("dev")["tag"], "v1.2.3-beta.7")
        self.assertEqual(self.plan()["tag"], "v1.2.3")
        self.assertEqual(self.plan()["publish"], "true")

    def test_pull_requests_cannot_publish(self):
        for branch in ("main", "dev"):
            self.assertEqual(
                self.plan(branch, event="pull_request")["publish"], "false"
            )

    def test_same_commit_can_retry(self):
        self.git("tag", "-a", "v1.2.3", "-m", "Production")
        self.assertEqual(self.plan()["publish"], "true")

    def test_production_version_cannot_move(self):
        self.git("tag", "v1.2.3")
        with (self.root / "Cargo.toml").open("a") as manifest:
            manifest.write('name = "latch"\n')
        self.commit("Package name")
        self.assertEqual(self.plan()["publish"], "false")

    def test_beta_version_cannot_move(self):
        self.git("tag", "v1.2.3-beta.7")
        with (self.root / "Cargo.toml").open("a") as manifest:
            manifest.write('name = "latch"\n')
        self.commit("Package name")
        self.plan("dev", succeeds=False)

    def test_invalid_release_inputs(self):
        self.plan("feature", succeeds=False)
        self.plan(run="0", succeeds=False)
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.2.3-beta.1"\n')
        self.plan(succeeds=False)


if __name__ == "__main__":
    unittest.main()
