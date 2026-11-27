import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "release_pr.py"
PR_URL = "https://github.com/jfortez/mssql-mcp/pull/42"
REPOSITORY = "jfortez/mssql-mcp"


class ReleasePrTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name) / "checkout"
        self.root.mkdir()
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "mssql-mcp"\nversion = "0.1.0"\nedition = "2024"\n'
            '[lib]\npath = "lib.rs"\n',
            encoding="utf-8",
        )
        (self.root / "lib.rs").touch()
        self.checked("cargo", "generate-lockfile", "--offline")
        self.checked("git", "init", "--quiet", "--initial-branch=master")
        self.checked("git", "add", ".")
        self.checked("git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                     "commit", "-qm", "feat: first change")
        self.sha = self.checked("git", "rev-parse", "--short", "HEAD")

    def checked(self, *args):
        return subprocess.check_output(args, cwd=self.root, text=True).strip()

    def assert_command(self, *args, stdout="", stderr="", status=0, env=None):
        run = subprocess.run([sys.executable, SCRIPT, *args], cwd=self.root, env=env,
                             text=True, capture_output=True)
        self.assertEqual((run.returncode, run.stdout, run.stderr), (status, stdout, stderr))

    def versions(self):
        manifest = tomllib.loads((self.root / "Cargo.toml").read_text(encoding="utf-8"))
        lock = tomllib.loads((self.root / "Cargo.lock").read_text(encoding="utf-8"))
        return manifest["package"]["version"], lock["package"][0]["version"]

    def remote_sha(self, branch):
        line = self.checked("git", "ls-remote", "origin", f"refs/heads/{branch}")
        return line.split("\t")[0] if line else ""

    def add_origin(self):
        origin = self.root.parent / "origin.git"
        self.checked("git", "init", "--bare", "--quiet", origin)
        self.checked("git", "remote", "add", "origin", origin)
        self.checked("git", "push", "--quiet", "origin", "master")
        bin_dir = self.root.parent / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(f"#!{sys.executable}\n" + '''
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
with open(os.environ["GH_CALLS"], "a") as calls:
    calls.write(json.dumps(args) + "\\n")
pr = Path(os.environ["GH_PR"])
if args[:2] == ["pr", "list"] and os.environ.get("GH_FORK_URL"):
    print(os.environ["GH_FORK_URL"])
    sys.exit(0)
if pr.exists():
    print(pr.read_text(), end="")
    sys.exit(0)
if args[:2] == ["pr", "create"]:
    if os.environ.get("GH_FAIL_CREATE") == "1":
        print("Resource not accessible by integration", file=sys.stderr)
        sys.exit(1)
    pr.write_text(os.environ["GH_URL"] + "\\n")
    print(pr.read_text(), end="")
''', encoding="utf-8")
        gh.chmod(0o755)
        self.calls = self.root.parent / "gh-calls.jsonl"
        return {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                "GH_CALLS": str(self.calls),
                "GH_PR": str(self.root.parent / "pr.txt"), "GH_URL": PR_URL}

    def test_first_release_adds_notes_without_bumping(self):
        self.assert_command("prepare", stdout="0.1.0\n")
        self.assertEqual(self.versions(), ("0.1.0", "0.1.0"))
        notes = (self.root / "releases/v0.1.0.md").read_text(encoding="utf-8")
        self.assertEqual(notes, f"# mssql-mcp v0.1.0\n\n## Changes\n\n- {self.sha} feat: first change\n")
        self.assertEqual(self.checked("git", "status", "--short"), "?? releases/")

    def test_explicit_version_updates_manifest_and_lock(self):
        self.assert_command("prepare", "0.1.1", stdout="0.1.1\n")
        self.assertEqual(self.versions(), ("0.1.1", "0.1.1"))
        self.assertTrue((self.root / "releases/v0.1.1.md").is_file())
        self.checked("cargo", "metadata", "--format-version", "1", "--locked", "--offline")

    def test_invalid_version_leaves_checkout_unchanged(self):
        self.assert_command("prepare", "../0.1.1", status=1,
                            stderr="Version must be in MAJOR.MINOR.PATCH format.\n")
        self.assertEqual(self.versions(), ("0.1.0", "0.1.0"))
        self.assertEqual(self.checked("git", "status", "--short"), "")
        self.assertEqual(self.checked("git", "rev-parse", "--short", "HEAD"), self.sha)

    def test_manual_publish_requires_a_candidate_on_the_default_branch(self):
        self.assert_command("notes", "0.1.0", "refs/heads/master", "master", status=1,
                            stderr="Merge the release PR for v0.1.0 before publishing.\n")
        self.assertEqual(self.checked("git", "status", "--short"), "")
        self.assert_command("prepare", stdout="0.1.0\n")
        before = self.checked("git", "status", "--short")
        self.assert_command("notes", "0.1.0", "refs/heads/release/v0.1.0", "master", status=1,
                            stderr="Dispatch Release from the default branch: master.\n")
        self.assertEqual(self.checked("git", "status", "--short"), before)
        self.assert_command("notes", "0.1.0", "refs/heads/master", "master",
                            stdout="releases/v0.1.0.md\n")

    def test_open_pushes_only_the_release_branch_and_does_not_duplicate_the_pr(self):
        env = {**self.add_origin(), "GH_FORK_URL": "https://github.com/jfortez/mssql-mcp/pull/99"}
        base = self.checked("git", "rev-parse", "HEAD")
        self.checked("git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                     "tag", "-a", "local-only", "-m", "local-only")
        self.checked("git", "config", "push.followTags", "true")

        self.assert_command("prepare", stdout="0.1.0\n")
        for _ in range(2):
            self.assert_command("open", "0.1.0", "master", REPOSITORY, stdout=f"{PR_URL}\n", env=env)
        first, create, last = (json.loads(line) for line in self.calls.read_text(encoding="utf-8").splitlines())
        self.assertEqual(first, last)
        self.assertEqual(first[:5], ["api", "--hostname", "github.com", "--method", "GET"])
        self.assertIn(f"repos/{REPOSITORY}/pulls", first)
        self.assertIn("base=master", first)
        self.assertIn("head=jfortez:release/v0.1.0", first)
        self.assertIn("state=open", first)
        self.assertIn(f'select(.head.repo.full_name == "{REPOSITORY}")', first[first.index("--jq") + 1])
        self.assertEqual(create[:2], ["pr", "create"])
        self.assertEqual(create[create.index("--repo") + 1], REPOSITORY)
        self.assertEqual(create[create.index("--base") + 1], "master")
        self.assertEqual(create[create.index("--head") + 1], "release/v0.1.0")
        self.assertEqual(self.checked("git", "ls-remote", "origin", "refs/heads/master"),
                         f"{base}\trefs/heads/master")
        release_ref = self.checked("git", "ls-remote", "origin", "refs/heads/release/v0.1.0")
        self.assertEqual(release_ref, f"{self.checked('git', 'rev-parse', 'HEAD')}\trefs/heads/release/v0.1.0")
        self.assertEqual(self.checked("git", "diff", "--name-only", base, "HEAD"), "releases/v0.1.0.md")
        self.assertEqual(self.checked("git", "ls-remote", "--tags", "origin"), "")

    def test_failed_pr_creation_rerun_reuses_the_matching_release_branch(self):
        env = self.add_origin()
        base = self.checked("git", "rev-parse", "HEAD")
        self.assert_command("prepare", stdout="0.1.0\n")
        self.assert_command("open", "0.1.0", "master", REPOSITORY, status=1,
                            stderr="Command failed: gh pr create\n"
                                   "Resource not accessible by integration\n",
                            env={**env, "GH_FAIL_CREATE": "1"})
        pushed = self.remote_sha("release/v0.1.0")
        self.assertEqual(pushed, self.checked("git", "rev-parse", "HEAD"))
        self.assert_command("open", "0.1.0", "master", REPOSITORY, stdout=f"{PR_URL}\n", env=env)
        self.assertEqual(self.remote_sha("release/v0.1.0"), pushed)
        self.assertEqual(self.checked("git", "ls-remote", "origin", "refs/heads/master"),
                         f"{base}\trefs/heads/master")
        self.assertEqual(len(self.calls.read_text(encoding="utf-8").splitlines()), 4)

    def test_open_refuses_a_release_branch_with_different_content(self):
        env = self.add_origin()
        self.assert_command("prepare", stdout="0.1.0\n")
        self.checked("git", "switch", "--quiet", "--create", "release/v0.1.0")
        self.checked("git", "add", "releases/v0.1.0.md")
        self.checked("git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                     "commit", "-qm", "release: v0.1.0")
        (self.root / "releases" / "unexpected.txt").write_text("stray\n", encoding="utf-8")
        self.checked("git", "add", "releases/unexpected.txt")
        self.checked("git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                     "commit", "-qm", "stray")
        self.checked("git", "push", "--quiet", "origin", "release/v0.1.0")
        pushed = self.remote_sha("release/v0.1.0")
        self.assert_command("open", "0.1.0", "master", REPOSITORY, status=1,
                            stderr="refs/heads/release/v0.1.0 already exists with different "
                                   "content; delete it or choose a new version.\n", env=env)
        self.assertEqual(self.remote_sha("release/v0.1.0"), pushed)
        pr_calls = [json.loads(line) for line in self.calls.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([call[0] for call in pr_calls], ["api"])
        self.assertFalse((self.root.parent / "pr.txt").exists())

    def test_open_refuses_a_release_branch_without_the_prepared_version_bump(self):
        env = self.add_origin()
        self.assert_command("prepare", "0.1.1", stdout="0.1.1\n")
        self.checked("git", "switch", "--quiet", "--create", "release/v0.1.1")
        self.checked("git", "add", "releases/v0.1.1.md")
        self.checked("git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                     "commit", "-qm", "release: notes only")
        self.checked("git", "push", "--quiet", "origin", "release/v0.1.1")
        pushed = self.remote_sha("release/v0.1.1")
        before = self.checked("git", "status", "--short")
        self.assert_command("open", "0.1.1", "master", REPOSITORY, status=1,
                            stderr="refs/heads/release/v0.1.1 already exists with different "
                                   "content; delete it or choose a new version.\n", env=env)
        self.assertEqual(self.remote_sha("release/v0.1.1"), pushed)
        self.assertEqual(self.checked("git", "status", "--short"), before)
        self.assertEqual(self.versions(), ("0.1.1", "0.1.1"))
        self.assertFalse((self.root.parent / "pr.txt").exists())

    def test_open_commits_the_selected_version_with_the_notes(self):
        env = self.add_origin()
        base = self.checked("git", "rev-parse", "HEAD")
        self.assert_command("prepare", "0.2.0", stdout="0.2.0\n")
        self.assert_command("open", "0.2.0", "master", REPOSITORY, stdout=f"{PR_URL}\n", env=env)
        self.assertEqual(self.checked("git", "diff", "--name-only", base, "HEAD"),
                         "Cargo.lock\nCargo.toml\nreleases/v0.2.0.md")
        self.assertEqual(self.versions(), ("0.2.0", "0.2.0"))
        self.assertEqual(self.remote_sha("release/v0.2.0"),
                         self.checked("git", "rev-parse", "HEAD"))

    def test_prepare_refuses_existing_notes(self):
        self.assert_command("prepare", stdout="0.1.0\n")
        self.assert_command("prepare", status=1,
                            stderr="releases/v0.1.0.md already exists; delete it to "
                                   "regenerate the notes for v0.1.0.\n")
        self.assertEqual(self.versions(), ("0.1.0", "0.1.0"))

    def test_prepare_refuses_an_existing_tag(self):
        self.checked("git", "tag", "v0.1.1")
        self.assert_command("prepare", "0.1.1", status=1,
                            stderr="Tag v0.1.1 already exists; choose a new version.\n")
        self.assertEqual(self.versions(), ("0.1.0", "0.1.0"))
        self.assertFalse((self.root / "releases").exists())

    def test_notes_refuse_an_empty_candidate(self):
        (self.root / "releases").mkdir()
        (self.root / "releases" / "v0.1.0.md").write_text(" \n", encoding="utf-8")
        self.assert_command("notes", "0.1.0", "refs/heads/master", "master", status=1,
                            stderr="Merge the release PR for v0.1.0 before publishing.\n")

    def test_later_version_notes_start_after_the_previous_tag(self):
        self.checked("git", "tag", "v0.1.0")
        (self.root / "later.txt").write_text("later\n", encoding="utf-8")
        self.checked("git", "add", "later.txt")
        self.checked("git", "-c", "user.name=Test", "-c", "user.email=test@example.com",
                     "commit", "-qm", "feat: second change")
        sha = self.checked("git", "rev-parse", "--short", "HEAD")
        self.assert_command("prepare", "0.2.0", stdout="0.2.0\n")
        notes = (self.root / "releases/v0.2.0.md").read_text(encoding="utf-8")
        self.assertEqual(notes, f"# mssql-mcp v0.2.0\n\n## Changes\n\n"
                                f"- {sha} feat: second change\n")
        self.assertEqual(self.versions(), ("0.2.0", "0.2.0"))


if __name__ == "__main__":
    unittest.main()
