#!/usr/bin/env python3
"""Prepare the release pull request and guard the manual publish."""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

MANIFEST = Path("Cargo.toml")
LOCKFILE = Path("Cargo.lock")
NOTES_DIR = Path("releases")
VERSION_PATTERN = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$", re.ASCII)
REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
MANIFEST_VERSION = re.compile(r'^version\s*=\s*"')
COMMIT_IDENTITY = (
    "-c",
    "user.name=github-actions[bot]",
    "-c",
    "user.email=41898282+github-actions[bot]@users.noreply.github.com",
)


class ReleaseError(Exception):
    """A release step refused to continue."""


def fail(message):
    """Stop with a message a maintainer can act on."""
    raise ReleaseError(message)


def run(command, *, check=True):
    """Run one command and return its completed process."""
    result = subprocess.run(command, text=True, capture_output=True)
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        message = f"Command failed: {' '.join(command)}"
        fail(f"{message}\n{detail}" if detail else message)
    return result


def credential_args():
    """Use the gh credential helper only for authenticated remote transports."""
    if not os.environ.get("GH_TOKEN"):
        return []
    remote = run(["git", "remote", "get-url", "origin"], check=False).stdout.strip()
    if "://" not in remote or remote.startswith("file://"):
        return []
    return ["-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential"]


def git_command(args, auth):
    """Build a git command, adding the gh credential helper when needed."""
    return ["git", *(credential_args() if auth else ()), *args]


def git(*args, auth=False, check=True):
    """Run one git command and return its stripped standard output."""
    return run(git_command(args, auth), check=check).stdout.strip()


def gh(*args):
    """Run one gh command and return its standard output."""
    result = subprocess.run(["gh", *args], text=True, capture_output=True)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        message = f"Command failed: gh {' '.join(args[:2])}"
        fail(f"{message}\n{detail}" if detail else message)
    return result.stdout


def first_url(output):
    """Return the first https URL printed by gh, if any."""
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("https://"):
            return line
    return ""


def version_tuple(value):
    """Turn MAJOR.MINOR.PATCH into comparable integers."""
    return tuple(int(part) for part in value.split("."))


def require_version(value):
    """Accept stable MAJOR.MINOR.PATCH versions only."""
    if not VERSION_PATTERN.fullmatch(value):
        fail("Version must be in MAJOR.MINOR.PATCH format.")
    return value


def notes_path(version):
    """Return the reviewed release notes for one version."""
    return NOTES_DIR / f"v{version}.md"


def manifest_data():
    """Read the workspace package name and version."""
    try:
        document = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(f"Could not read {MANIFEST}: {error}")
    package = document.get("package") if isinstance(document, dict) else None
    if not isinstance(package, dict):
        fail(f"Could not read the package table from {MANIFEST}.")
    name, version = package.get("name"), package.get("version")
    if not isinstance(name, str) or not isinstance(version, str):
        fail(f"Could not read the package name and version from {MANIFEST}.")
    return name, version


def previous_tag(version):
    """Return the highest local tag below the requested version."""
    target = version_tuple(version)
    previous = None
    for tag in git("tag", "--list", "v*", "--merged", "HEAD").splitlines():
        candidate = tag.removeprefix("v")
        if not VERSION_PATTERN.fullmatch(candidate):
            continue
        if version_tuple(candidate) < target and (
            previous is None or version_tuple(candidate) > version_tuple(previous)
        ):
            previous = candidate
    return previous


def release_notes(name, version):
    """Render reviewed notes from the commits since the previous local tag."""
    previous = previous_tag(version)
    revision = f"v{previous}..HEAD" if previous else "HEAD"
    changes = git("log", "--pretty=format:- %h %s", revision)
    body = changes if changes else "No commits since the previous release."
    return f"# {name} v{version}\n\n## Changes\n\n{body}\n"


def bumped_manifest(text, version):
    """Rewrite the [package] version line and leave every other byte alone."""
    lines = text.splitlines(keepends=True)
    in_package = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
            continue
        if in_package and MANIFEST_VERSION.match(stripped):
            lines[index] = f'version = "{version}"\n'
            return "".join(lines)
    fail(f"Could not find the package version in {MANIFEST}.")


def prepare(selected_version):
    """Write the release notes and bump the workspace only when selected."""
    name, current = manifest_data()
    require_version(current)
    target = require_version(selected_version or current)
    if version_tuple(target) < version_tuple(current):
        fail(f"Version {target} is lower than the manifest version {current}.")
    notes = notes_path(target)
    if notes.exists():
        fail(
            f"{notes.as_posix()} already exists; "
            f"delete it to regenerate the notes for v{target}."
        )
    tag = f"v{target}"
    if git("tag", "--list", tag):
        fail(f"Tag {tag} already exists; choose a new version.")
    text = release_notes(name, target)
    manifest_text = MANIFEST.read_text(encoding="utf-8")
    lock_text = LOCKFILE.read_text(encoding="utf-8") if LOCKFILE.exists() else None
    if target != current:
        MANIFEST.write_text(bumped_manifest(manifest_text, target), encoding="utf-8")
        update = run(["cargo", "update", "--workspace"], check=False)
        if update.returncode != 0:
            MANIFEST.write_text(manifest_text, encoding="utf-8")
            if lock_text is not None:
                LOCKFILE.write_text(lock_text, encoding="utf-8")
            detail = (update.stderr or update.stdout).strip()
            message = "Command failed: cargo update --workspace"
            fail(f"{message}\n{detail}" if detail else message)
    notes.parent.mkdir(parents=True, exist_ok=True)
    notes.write_text(text, encoding="utf-8")
    print(target)


def require_notes(version, ref, default_branch):
    """Require nonempty reviewed notes dispatched from the default branch."""
    require_version(version)
    if ref != f"refs/heads/{default_branch}":
        fail(f"Dispatch Release from the default branch: {default_branch}.")
    path = notes_path(version)
    if not path.is_file() or not path.read_text(encoding="utf-8").strip():
        fail(f"Merge the release PR for v{version} before publishing.")
    print(path.as_posix())


def remote_branch_sha(branch):
    """Return the origin commit for one branch."""
    result = run(
        git_command(("ls-remote", "--exit-code", "origin", f"refs/heads/{branch}"), True),
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.strip()
        message = f"Could not read refs/heads/{branch} from origin."
        fail(f"{message}\n{detail}" if detail else message)
    return result.stdout.split()[0]


def branch_matches(commit, base, notes):
    """Require one commit on base with exactly the prepared changes."""
    if git("rev-list", "--parents", "-n", "1", commit).split() != [commit, base]:
        return False
    paths = set(git("diff", "--name-only", base, commit).splitlines())
    version_paths = git("diff", "--name-only", base, "--", str(MANIFEST), str(LOCKFILE)).splitlines()
    if paths != {notes.as_posix(), *version_paths}:
        return False
    for path in paths:
        committed = git("rev-parse", "--verify", "--quiet", f"{commit}:{path}", check=False)
        if committed != git("hash-object", "--", path, check=False):
            return False
    return True


def collision(branch):
    """Refuse to overwrite a release branch that does not match the prepared tree."""
    fail(
        f"refs/heads/{branch} already exists with different content; "
        "delete it or choose a new version."
    )


def commit_release_branch(version, branch, base, default_branch, notes):
    """Commit only the prepared files on release/v<version> and return its commit."""
    if git("rev-parse", "HEAD") != base:
        fail(
            f"Check out the latest {default_branch} "
            "before opening the release pull request."
        )
    intended = {notes.as_posix(), MANIFEST.as_posix(), LOCKFILE.as_posix()}
    staged = git("diff", "--cached", "--name-only", "HEAD").splitlines()
    unrelated = [path for path in staged if path not in intended]
    if unrelated:
        fail(
            "Unrelated staged changes: "
            + ", ".join(unrelated)
            + "; unstage them and rerun."
        )
    git("add", "--", notes.as_posix(), MANIFEST.as_posix(), LOCKFILE.as_posix())
    git("switch", "--quiet", "--create", branch)
    git(*COMMIT_IDENTITY, "commit", "--quiet", "-m", f"chore(release): v{version}")
    return git("rev-parse", "HEAD")


def open_release_pr(version, default_branch, repository):
    """Open one same-repository release pull request from release/v<version>."""
    require_version(version)
    if not REPOSITORY_PATTERN.fullmatch(repository):
        fail("Repository must be in owner/name format.")
    if not default_branch or any(character.isspace() for character in default_branch):
        fail("Default branch must name a branch.")
    branch = f"release/v{version}"
    notes = notes_path(version)
    existing = first_url(
        gh(
            "api", "--hostname", "github.com", "--method", "GET", f"repos/{repository}/pulls",
            "-f", f"base={default_branch}", "-f", f"head={repository.split('/')[0]}:{branch}",
            "-f", "state=open", "--jq",
            f'.[] | select(.head.repo.full_name == "{repository}") | .html_url',
        )
    )
    if existing:
        print(existing)
        return
    if not notes.is_file() or not notes.read_text(encoding="utf-8").strip():
        fail(f"Run prepare for v{version} before opening the release pull request.")
    base = remote_branch_sha(default_branch)
    remote = git("ls-remote", "origin", f"refs/heads/{branch}", auth=True)
    if remote:
        git("fetch", "--quiet", "--no-tags", "origin", f"refs/heads/{branch}", auth=True)
        head = remote.split()[0]
        if not branch_matches(head, base, notes):
            collision(branch)
    else:
        head = git(
            "rev-parse", "--verify", "--quiet", f"refs/heads/{branch}", check=False
        )
        if head:
            if not branch_matches(head, base, notes):
                collision(branch)
        else:
            head = commit_release_branch(version, branch, base, default_branch, notes)
        git("push", "--quiet", "--no-follow-tags", "origin", f"{head}:refs/heads/{branch}", auth=True)
    body = (
        notes.read_text(encoding="utf-8").rstrip()
        + "\n\nMerge this pull request, then run the **Release** workflow to publish.\n"
    )
    created = first_url(
        gh(
            "pr",
            "create",
            "--repo",
            repository,
            "--base",
            default_branch,
            "--head",
            branch,
            "--title",
            f"Release v{version}",
            "--body",
            body,
        )
    )
    if not created:
        fail("Could not read the pull request URL from gh.")
    print(created)


def main(argv):
    """Dispatch one release helper command."""
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_command = commands.add_parser(
        "prepare", help="write the release notes and optionally bump the version"
    )
    prepare_command.add_argument(
        "version", nargs="?", help="version to release; defaults to the manifest version"
    )
    notes_command = commands.add_parser(
        "notes", help="require the reviewed notes on the default branch"
    )
    notes_command.add_argument("version")
    notes_command.add_argument("ref")
    notes_command.add_argument("default_branch")
    open_command = commands.add_parser(
        "open", help="open the release pull request from release/v<version>"
    )
    open_command.add_argument("version")
    open_command.add_argument("default_branch")
    open_command.add_argument("repository")
    args = parser.parse_args(argv)
    try:
        if args.command == "prepare":
            prepare(args.version)
        elif args.command == "notes":
            require_notes(args.version, args.ref, args.default_branch)
        else:
            open_release_pr(args.version, args.default_branch, args.repository)
    except ReleaseError as error:
        print(error, file=sys.stderr)
        return 1
    except OSError as error:
        print(f"Command failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
