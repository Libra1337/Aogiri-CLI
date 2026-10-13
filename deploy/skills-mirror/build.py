#!/usr/bin/env python3
"""Builds the LynShen skills mirror (docs/software-library.md, "Skills mirror").

  deploy/skills-mirror/build.py build <out-dir>
  deploy/skills-mirror/build.py unchanged <out-dir> < latest.json

`build` writes <out-dir>/catalog.json and one <source>__<id>.tar.gz per skill
the mirror may redistribute, ready for
`software-release sign <key> skills <version> <out-dir>`. Anthropic's skills
are read at the commit the engine's bundled index pins
(crates/agent-core/src/anthropic-skills.json), with its names, tags and
license classes; each community repository in sources.json at its current
commit, the same way the engine lists it (skills/agent-core
`fetch_github_source`). A skill is packed only with a license: its own
LICENSE/COPYING file, else the repository's, which goes into the archive.
Skills without one, or that Anthropic does not let others redistribute, are
listed only; the engine installs those from GitHub as before. Archives are
reproducible: the same commit gives the same bytes.

`unchanged` exits 0 when every file in <out-dir> is already published with
the same sha256 (the manifest on stdin), so a scheduled run publishes only
when a source changed.

GITHUB_TOKEN, when set, raises GitHub's API limit (four calls per build).
"""
from __future__ import annotations

import calendar
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.request
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[2]
ANTHROPIC_INDEX = ROOT / 'crates' / 'agent-core' / 'src' / 'anthropic-skills.json'
SOURCES = Path(__file__).resolve().with_name('sources.json')

# The engine's limits (crates/agent-core/src/skills.rs): a skill that breaks
# one is listed without an archive.
MAX_ARCHIVE_BYTES = 20 * 1024 * 1024
MAX_FILE_BYTES = 20 * 1024 * 1024
MAX_SKILL_BYTES = 100 * 1024 * 1024
MAX_SKILL_FILES = 4096
# A repository's tarball, and its files on disk: a repository larger than
# this is not mirrored (the build fails), rather than fill the disk.
MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024
MAX_REPOSITORY_BYTES = 1024 * 1024 * 1024

SKILL_ID = re.compile(r'[a-z0-9](?:[a-z0-9-]*[a-z0-9])?')
FOLDER = re.compile(r'[A-Za-z0-9_-][A-Za-z0-9._-]*')
OWNER_REPO = re.compile(r'https://github\.com/([A-Za-z0-9._-]+)/([A-Za-z0-9._-]+)')
LICENSE_FILE = re.compile(r'(LICEN[CS]E|COPYING)([._-].*)?', re.IGNORECASE)
NOTICE_FILE = re.compile(r'(NOTICE|THIRD[_-]PARTY[_-]NOTICES)([._-].*)?', re.IGNORECASE)


def log(message: str) -> None:
    print(message, file=sys.stderr)


# ---------- GitHub ----------

def get(url: str, accept: str | None = None, limit: int = MAX_DOWNLOAD_BYTES) -> bytes:
    request = urllib.request.Request(url, headers={'User-Agent': 'lynshen-skills-mirror'})
    if accept:
        request.add_header('Accept', accept)
    token = os.environ.get('GITHUB_TOKEN', '').strip()
    if token and url.startswith('https://api.github.com/'):
        request.add_header('Authorization', f'Bearer {token}')
    with urllib.request.urlopen(request, timeout=120) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f'{url}: more than {limit} bytes')
    return data


def commit(owner: str, repo: str, ref: str) -> tuple[str, int]:
    """The commit `ref` names and its commit time (seconds)."""
    value = json.loads(get(f'https://api.github.com/repos/{owner}/{repo}/commits/{ref}',
                           'application/vnd.github+json'))
    sha = value['sha']
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError(f'{owner}/{repo}: unexpected commit {sha!r}')
    when = time.strptime(value['commit']['committer']['date'], '%Y-%m-%dT%H:%M:%SZ')
    return sha, calendar.timegm(when)


class Tree:
    """A repository's files at one commit: `files` maps each regular file's
    path to whether it is executable; `links` holds every other entry
    (symbolic links, and files too big for the engine), which the engine
    does not install."""

    def __init__(self, root: Path):
        self.root = root
        self.files: dict[str, bool] = {}
        self.links: set[str] = set()

    @classmethod
    def download(cls, owner: str, repo: str, sha: str, dest: Path) -> 'Tree':
        data = get(f'https://codeload.github.com/{owner}/{repo}/tar.gz/{sha}')
        return cls.from_tarball(data, dest, f'{owner}/{repo}')

    @classmethod
    def from_tarball(cls, data: bytes, dest: Path, label: str) -> 'Tree':
        """GitHub's tarball (`<repo>-<sha>/<files>`) unpacked into `dest`:
        regular files only, none outside `dest`, none over the engine's file
        limit, and no more than MAX_REPOSITORY_BYTES in all."""
        tree = cls(dest)
        total = 0
        with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
            for member in archive:
                parts = PurePosixPath(member.name).parts[1:]
                if not parts or member.isdir():
                    continue
                if any(part in ('', '.', '..') for part in parts) or PurePosixPath(member.name).is_absolute():
                    raise ValueError(f'{label}: unsafe path {member.name}')
                path = '/'.join(parts)
                if not member.isfile() or member.size > MAX_FILE_BYTES:
                    tree.links.add(path)
                    continue
                total += member.size
                if total > MAX_REPOSITORY_BYTES:
                    raise ValueError(f'{label}: more than {MAX_REPOSITORY_BYTES} bytes of files')
                target = dest.joinpath(*parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as source, target.open('wb') as output:
                    shutil.copyfileobj(source, output)
                tree.files[path] = bool(member.mode & 0o111)
        return tree

    @classmethod
    def from_dir(cls, root: Path) -> 'Tree':
        """A checked-out folder (for tests)."""
        tree = cls(root)
        for path in sorted(root.rglob('*')):
            relative = path.relative_to(root).as_posix()
            if path.is_symlink():
                tree.links.add(relative)
            elif path.is_file():
                tree.files[relative] = bool(path.stat().st_mode & 0o111)
        return tree

    def read(self, path: str) -> bytes:
        return self.root.joinpath(*path.split('/')).read_bytes()


# ---------- what the engine reads ----------

def frontmatter(text: str, key: str) -> str:
    """`read_frontmatter_field` of crates/agent-core/src/prompt.rs."""
    lines = text.splitlines()
    if not lines or lines[0].rstrip() != '---':
        return ''
    index = 1
    while index < len(lines):
        line = lines[index]
        index += 1
        if line.rstrip() == '---':
            return ''
        if line.startswith((' ', '\t')) or ':' not in line:
            continue
        field, value = line.split(':', 1)
        if field.strip() != key:
            continue
        value = value.strip()
        if value.startswith(('>', '|')) and len(value) <= 2:
            parts = []
            while index < len(lines) and (not lines[index].strip() or lines[index].startswith((' ', '\t'))):
                parts.append(lines[index].strip())
                index += 1
            return (' ' if value.startswith('>') else '\n').join(parts).strip()
        return value.strip('"').strip("'")
    return ''


def find_skills(tree: Tree) -> tuple[str, list[str]]:
    """`fetch_github_source`'s choice: every `<id>/SKILL.md` at the top or
    `<dir>/<id>/SKILL.md` one level down, from the folder holding the most,
    `skills/` and the top level first."""
    by_dir: dict[str, list[str]] = {}
    for path in tree.files:
        parts = path.split('/')
        if len(parts) == 2 and parts[1] == 'SKILL.md':
            folder, skill = '', parts[0]
        elif len(parts) == 3 and parts[2] == 'SKILL.md':
            folder, skill = parts[0], parts[1]
        else:
            continue
        if SKILL_ID.fullmatch(skill):
            by_dir.setdefault(folder, []).append(skill)
    best = None
    for folder in sorted(by_dir):
        rank = (folder in ('skills', ''), len(by_dir[folder]))
        if best is None or rank >= best[0]:
            best = (rank, folder)
    if best is None:
        raise ValueError('no SKILL.md found')
    return best[1], sorted(by_dir[best[1]])


def license_label(text: str) -> str:
    """A short name for a license file's text, for the catalog."""
    if 'Apache License' in text and 'Version 2.0' in text:
        return 'Apache-2.0'
    if 'MIT License' in text or 'Permission is hereby granted, free of charge' in text:
        return 'MIT'
    if 'GNU GENERAL PUBLIC LICENSE' in text:
        return 'GPL'
    if 'Redistribution and use in source and binary forms' in text:
        return 'BSD'
    if 'Creative Commons' in text:
        return 'CC'
    return 'see LICENSE'


# ---------- packing ----------

def skill_files(tree: Tree, prefix: str) -> tuple[list[str], list[str]]:
    """The files under `prefix/` and the links among them."""
    files = sorted(path for path in tree.files if path.startswith(prefix + '/'))
    links = sorted(path for path in tree.links if path.startswith(prefix + '/'))
    return files, links


def legal_files(tree: Tree, files: list[str], prefix: str) -> tuple[list[tuple[str, str]], str | None]:
    """The repository's top-level license and notice files a skill folder
    lacks, as (name in the archive, path in the repository), and the license
    text that applies (None: no license, so no archive)."""
    own = {path[len(prefix) + 1:] for path in files if '/' not in path[len(prefix) + 1:]}
    own_license = sorted(name for name in own if LICENSE_FILE.fullmatch(name))
    top = sorted(path for path in tree.files if '/' not in path)
    extra = []
    license_text = None
    if own_license:
        license_text = tree.read(f'{prefix}/{own_license[0]}').decode('utf-8', 'replace')
    else:
        top_license = [path for path in top if LICENSE_FILE.fullmatch(path)]
        if top_license:
            extra.extend((path, path) for path in top_license)
            license_text = tree.read(top_license[0]).decode('utf-8', 'replace')
    if license_text is not None and not any(NOTICE_FILE.fullmatch(name) for name in own):
        extra.extend((path, path) for path in top if NOTICE_FILE.fullmatch(path))
    return extra, license_text


def pack(tree: Tree, prefix: str, skill: str, extra: list[tuple[str, str]], mtime: int) -> bytes:
    """`<skill>/<files>` as a reproducible tar.gz: sorted, owner root, the
    commit's time, 0644 or 0755."""
    files, _ = skill_files(tree, prefix)
    entries = [(f'{skill}/{path[len(prefix) + 1:]}', path) for path in files]
    entries += [(f'{skill}/{name}', path) for name, path in extra]
    entries.sort()
    if len(entries) > MAX_SKILL_FILES:
        raise ValueError(f'more than {MAX_SKILL_FILES} files')
    buffer = io.BytesIO()
    total = 0
    with gzip.GzipFile(filename='', mode='wb', fileobj=buffer, mtime=0, compresslevel=9) as compressed:
        with tarfile.open(fileobj=compressed, mode='w', format=tarfile.GNU_FORMAT) as archive:
            for name, path in entries:
                data = tree.read(path)
                total += len(data)
                if len(data) > MAX_FILE_BYTES or total > MAX_SKILL_BYTES:
                    raise ValueError(f'{path} breaks the engine size limits')
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mtime = mtime
                info.mode = 0o755 if tree.files[path] else 0o644
                info.uid = info.gid = 0
                info.uname = info.gname = ''
                archive.addfile(info, io.BytesIO(data))
    data = buffer.getvalue()
    if len(data) > MAX_ARCHIVE_BYTES:
        raise ValueError(f'archive exceeds {MAX_ARCHIVE_BYTES} bytes')
    return data


def archive_name(key: str, skill: str) -> str:
    return f"{key.replace('/', '_')}__{skill}.tar.gz"


def add_archive(entry: dict, tree: Tree, prefix: str, key: str, mtime: int, out: Path) -> str | None:
    """Packs one skill into `out` and names the archive in its catalog
    entry; returns the license text, or None when it stays listed only."""
    files, links = skill_files(tree, prefix)
    skill = entry['id']
    if f'{prefix}/SKILL.md' not in files:
        log(f'  {key} {skill}: no SKILL.md, listed only')
        return None
    if links:
        log(f'  {key} {skill}: has links or oversized files ({links[0]}), listed only')
        return None
    extra, license_text = legal_files(tree, files, prefix)
    if license_text is None:
        log(f'  {key} {skill}: no license, listed only')
        return None
    try:
        data = pack(tree, prefix, skill, extra, mtime)
    except ValueError as error:
        log(f'  {key} {skill}: {error}, listed only')
        return None
    name = archive_name(key, skill)
    (out / name).write_bytes(data)
    entry['archive'] = name
    return license_text


# ---------- sources ----------

def anthropic_source(index: dict, tree: Tree, sha: str, mtime: int, out: Path) -> dict:
    """The bundled index's skills at its pinned commit, as the engine lists
    them; archives only for the ones it marks redistributable."""
    skills = []
    for item in index['skills']:
        entry = {key: item[key] for key in ('id', 'name', 'description', 'tags', 'license', 'redistributable')}
        if entry['redistributable']:
            add_archive(entry, tree, f"skills/{item['id']}", 'anthropic', mtime, out)
        skills.append(entry)
    return {'name': 'Anthropic', 'repository': index['repository'], 'revision': sha, 'dir': 'skills',
            'skills': skills}


def community_source(name: str, repository: str, tree: Tree, sha: str, mtime: int, out: Path) -> dict:
    owner, repo = OWNER_REPO.fullmatch(repository).groups()
    folder, ids = find_skills(tree)
    skills = []
    for skill in ids:
        prefix = f'{folder}/{skill}' if folder else skill
        text = tree.read(f'{prefix}/SKILL.md').decode('utf-8', 'replace')
        entry = {
            'id': skill,
            'name': frontmatter(text, 'name') or skill,
            'description': frontmatter(text, 'description'),
            'tags': [],
            'license': '',
            'redistributable': True,
        }
        license_text = add_archive(entry, tree, prefix, f'{owner}/{repo}', mtime, out)
        if license_text is not None:
            entry['license'] = license_label(license_text)
        skills.append(entry)
    return {'name': name, 'repository': repository, 'revision': sha, 'dir': folder, 'skills': skills}


def check_sources(sources: list[dict]) -> None:
    for source in sources:
        if not OWNER_REPO.fullmatch(source['repository']) or not source['name'].strip():
            raise ValueError(f'bad source {source!r}')


def build(out: Path) -> None:
    out.mkdir(parents=True, exist_ok=True)
    if any(out.iterdir()):
        raise SystemExit(f'{out} is not empty')
    index = json.loads(ANTHROPIC_INDEX.read_text())
    community = json.loads(SOURCES.read_text())
    check_sources(community)
    catalog = {'format': 1, 'sources': []}
    with tempfile.TemporaryDirectory(prefix='skills-mirror-') as scratch:
        plan = [(None, index['repository'], index['revision'])]
        plan += [(source['name'], source['repository'], 'HEAD') for source in community]
        for at, (name, repository, ref) in enumerate(plan):
            owner, repo = OWNER_REPO.fullmatch(repository).groups()
            sha, mtime = commit(owner, repo, ref)
            log(f'{owner}/{repo} at {sha}')
            tree = Tree.download(owner, repo, sha, Path(scratch) / str(at))
            if name is None:
                if sha != index['revision']:
                    raise ValueError('the bundled Anthropic index is not pinned to a commit')
                catalog['sources'].append(anthropic_source(index, tree, sha, mtime, out))
            else:
                catalog['sources'].append(community_source(name, repository, tree, sha, mtime, out))
    write_catalog(catalog, out)


def write_catalog(catalog: dict, out: Path) -> None:
    text = json.dumps(catalog, ensure_ascii=False, indent=1) + '\n'
    (out / 'catalog.json').write_text(text, encoding='utf-8')
    archives = sum(1 for source in catalog['sources'] for skill in source['skills'] if 'archive' in skill)
    listed = sum(len(source['skills']) for source in catalog['sources'])
    log(f'catalog: {listed} skills, {archives} archives')


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def unchanged(out: Path, manifest: str) -> bool:
    """Whether the published release lists exactly these files and hashes."""
    try:
        release = json.loads(json.loads(manifest)['release'])
        published = {entry['name']: entry['sha256'] for entry in release['files']}
    except (ValueError, KeyError, TypeError):
        return False
    local = {path.name: sha256(path) for path in out.iterdir() if path.is_file()}
    return published == local


def main(argv: list[str]) -> int:
    if len(argv) == 3 and argv[1] == 'build':
        build(Path(argv[2]))
        return 0
    if len(argv) == 3 and argv[1] == 'unchanged':
        return 0 if unchanged(Path(argv[2]), sys.stdin.read()) else 1
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == '__main__':
    sys.exit(main(sys.argv))
