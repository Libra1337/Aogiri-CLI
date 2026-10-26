"""Offline tests for deploy/skills-mirror/build.py: python3 deploy/skills-mirror/test_build.py"""
import importlib.util
import io
import json
import os
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('skills_mirror_build', HERE / 'build.py')
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)

SHA = '0123456789abcdef0123456789abcdef01234567'
MIT = 'MIT License\n\nPermission is hereby granted, free of charge, to any person\n'
APACHE = 'Apache License\nVersion 2.0, January 2004\n'


def write(root: Path, path: str, text: str, mode: int = 0o644) -> None:
    target = root / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    target.chmod(mode)


def members(data: bytes) -> dict:
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        return {m.name: (m.mode, archive.extractfile(m).read().decode()) for m in archive if m.isfile()}


class BuildTest(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.root = Path(self.scratch.name)
        self.out = self.root / 'out'
        self.out.mkdir()

    def tearDown(self):
        self.scratch.cleanup()

    def repo(self, name: str) -> Path:
        path = self.root / name
        path.mkdir()
        return path

    def test_frontmatter_reads_fields_like_the_engine(self):
        text = '---\nname: "Demo"\ndescription: >-\n  Two\n  lines\nother: x\n---\nbody\n'
        self.assertEqual(build.frontmatter(text, 'name'), 'Demo')
        self.assertEqual(build.frontmatter(text, 'description'), 'Two lines')
        self.assertEqual(build.frontmatter(text, 'missing'), '')
        self.assertEqual(build.frontmatter('no frontmatter', 'name'), '')

    def test_skills_come_from_the_folder_holding_the_most(self):
        repo = self.repo('pick')
        for path in ['skills/a/SKILL.md', 'skills/b/SKILL.md', 'vendor/x/SKILL.md', 'vendor/y/SKILL.md',
                     'vendor/z/SKILL.md', 'skills/Bad_Name/SKILL.md', 'deep/er/c/SKILL.md']:
            write(repo, path, '---\nname: n\n---\n')
        self.assertEqual(build.find_skills(build.Tree.from_dir(repo)), ('skills', ['a', 'b']))
        top = self.repo('top')
        write(top, 'one/SKILL.md', 'x')
        self.assertEqual(build.find_skills(build.Tree.from_dir(top)), ('', ['one']))

    def test_licensed_skills_are_packed_with_their_license_and_others_are_listed_only(self):
        repo = self.repo('community')
        write(repo, 'LICENSE', MIT)
        write(repo, 'NOTICE', 'notice\n')
        write(repo, 'skills/alpha/SKILL.md', '---\nname: Alpha\ndescription: First skill\n---\n')
        write(repo, 'skills/alpha/scripts/run.sh', 'echo ok\n', 0o755)
        write(repo, 'skills/beta/SKILL.md', '---\ndescription: Own license\n---\n')
        write(repo, 'skills/beta/LICENSE.txt', APACHE)
        write(repo, 'skills/linked/SKILL.md', '---\nname: Linked\n---\n')
        os.symlink('../alpha/SKILL.md', repo / 'skills/linked/other.md')
        tree = build.Tree.from_dir(repo)

        source = build.community_source('Example', 'https://github.com/example/skills', tree, SHA, 1700000000,
                                        self.out)

        self.assertEqual(source['revision'], SHA)
        self.assertEqual(source['dir'], 'skills')
        alpha, beta, linked = source['skills']
        self.assertEqual((alpha['name'], alpha['description'], alpha['license']), ('Alpha', 'First skill', 'MIT'))
        self.assertEqual(alpha['archive'], 'example_skills__alpha.tar.gz')
        self.assertEqual(beta['name'], 'beta')
        self.assertEqual(beta['license'], 'Apache-2.0')
        self.assertNotIn('archive', linked)
        self.assertTrue(all(skill['redistributable'] for skill in source['skills']))

        files = members((self.out / alpha['archive']).read_bytes())
        self.assertEqual(sorted(files), ['alpha/LICENSE', 'alpha/NOTICE', 'alpha/SKILL.md', 'alpha/scripts/run.sh'])
        self.assertEqual(files['alpha/LICENSE'][1], MIT)
        self.assertEqual(files['alpha/scripts/run.sh'][0], 0o755)
        self.assertEqual(files['alpha/SKILL.md'][0], 0o644)
        files = members((self.out / beta['archive']).read_bytes())
        self.assertEqual(sorted(files), ['beta/LICENSE.txt', 'beta/NOTICE', 'beta/SKILL.md'])

        # The same commit packs to the same bytes.
        again = build.pack(tree, 'skills/alpha', 'alpha', [('LICENSE', 'LICENSE'), ('NOTICE', 'NOTICE')], 1700000000)
        self.assertEqual(again, (self.out / alpha['archive']).read_bytes())

        unlicensed = self.repo('unlicensed')
        write(unlicensed, 'skills/gamma/SKILL.md', '---\nname: Gamma\n---\n')
        source = build.community_source('Other', 'https://github.com/other/skills',
                                        build.Tree.from_dir(unlicensed), SHA, 0, self.out)
        self.assertEqual(source['skills'][0]['license'], '')
        self.assertNotIn('archive', source['skills'][0])

        # Terms of its own that are not open: listed with them, never packed.
        closed = self.repo('closed')
        write(closed, 'LICENSE', MIT)
        write(closed, 'skills/docx/SKILL.md',
              '---\nname: docx\nlicense: Proprietary. LICENSE.txt has complete terms\n---\n')
        source = build.community_source('Closed', 'https://github.com/closed/skills',
                                        build.Tree.from_dir(closed), SHA, 0, self.out)
        docx = source['skills'][0]
        self.assertEqual((docx['license'], docx['redistributable']),
                         ('Proprietary. LICENSE.txt has complete terms', False))
        self.assertNotIn('archive', docx)

    def test_anthropic_skills_keep_the_index_and_only_redistributable_ones_are_packed(self):
        repo = self.repo('anthropic')
        for skill in ('open', 'closed'):
            write(repo, f'skills/{skill}/SKILL.md', f'---\nname: {skill}\n---\n')
            write(repo, f'skills/{skill}/LICENSE.txt', APACHE)
        index = {'repository': 'https://github.com/anthropics/skills', 'revision': SHA, 'skills': [
            {'id': 'open', 'name': 'Open', 'description': 'd', 'tags': ['t'], 'license': 'Apache-2.0',
             'redistributable': True},
            {'id': 'closed', 'name': 'Closed', 'description': 'd', 'tags': [], 'license': 'Proprietary',
             'redistributable': False},
        ]}
        source = build.anthropic_source(index, build.Tree.from_dir(repo), SHA, 0, self.out)
        self.assertEqual(source['skills'][0]['archive'], 'anthropic__open.tar.gz')
        self.assertEqual(source['skills'][0]['tags'], ['t'])
        self.assertNotIn('archive', source['skills'][1])
        self.assertEqual(sorted(path.name for path in self.out.iterdir()), ['anthropic__open.tar.gz'])

    def test_a_build_is_unchanged_only_when_every_file_is_published_as_is(self):
        build.write_catalog({'format': 1, 'sources': []}, self.out)
        (self.out / 'a.tar.gz').write_bytes(b'a')
        files = [{'name': path.name, 'sha256': build.sha256(path)} for path in self.out.iterdir()]
        manifest = json.dumps({'release': json.dumps({'product': 'skills', 'version': '1.0.0', 'files': files})})
        self.assertTrue(build.unchanged(self.out, manifest))
        (self.out / 'a.tar.gz').write_bytes(b'b')
        self.assertFalse(build.unchanged(self.out, manifest))
        self.assertFalse(build.unchanged(self.out, 'not json'))

    def test_the_sources_are_github_repositories(self):
        build.check_sources(json.loads(build.SOURCES.read_text()))
        with self.assertRaises(ValueError):
            build.check_sources([{'name': 'x', 'repository': 'https://example.com/a/b'}])

    def tarball(self, entries) -> bytes:
        """GitHub's layout: every path under `repo-sha/`."""
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w:gz', format=tarfile.PAX_FORMAT) as archive:
            for name, data, kind in entries:
                info = tarfile.TarInfo(f'repo-{SHA}/{name}')
                info.type = kind
                if kind == tarfile.SYMTYPE:
                    info.linkname = '../../outside'
                    archive.addfile(info)
                else:
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
        return buffer.getvalue()

    def test_a_repository_tarball_unpacks_regular_files_only_within_limits(self):
        old = build.MAX_FILE_BYTES, build.MAX_REPOSITORY_BYTES
        self.addCleanup(lambda: (setattr(build, 'MAX_FILE_BYTES', old[0]),
                                 setattr(build, 'MAX_REPOSITORY_BYTES', old[1])))
        build.MAX_FILE_BYTES, build.MAX_REPOSITORY_BYTES = 100, 250
        data = self.tarball([
            ('skills/a/SKILL.md', b'---\nname: a\n---\n', tarfile.REGTYPE),
            ('skills/a/big.bin', b'x' * 101, tarfile.REGTYPE),
            ('skills/a/link.md', b'', tarfile.SYMTYPE),
        ])
        dest = self.root / 'tree'
        tree = build.Tree.from_tarball(data, dest, 'example/skills')
        self.assertEqual(sorted(tree.files), ['skills/a/SKILL.md'])
        self.assertEqual(sorted(tree.links), ['skills/a/big.bin', 'skills/a/link.md'])
        self.assertFalse((dest / 'skills/a/big.bin').exists())
        self.assertFalse((dest / 'skills/a/link.md').exists())
        # A skill with a file the engine would refuse is listed only.
        write(dest, 'LICENSE', MIT)
        tree.files['LICENSE'] = False
        entry = {'id': 'a'}
        self.assertIsNone(build.add_archive(entry, tree, 'skills/a', 'example/skills', 0, self.out))
        self.assertNotIn('archive', entry)

        with self.assertRaisesRegex(ValueError, 'unsafe path'):
            build.Tree.from_tarball(self.tarball([('skills/../../x', b'x', tarfile.REGTYPE)]),
                                    self.root / 'unsafe', 'example/skills')
        with self.assertRaisesRegex(ValueError, 'more than 250 bytes'):
            build.Tree.from_tarball(self.tarball([(f'f{n}', b'x' * 100, tarfile.REGTYPE) for n in range(3)]),
                                    self.root / 'large', 'example/skills')


if __name__ == '__main__':
    unittest.main()
