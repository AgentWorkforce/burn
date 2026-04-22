import { strict as assert } from 'node:assert';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import {
  __resetResolveProjectCacheForTesting,
  canonicalizeRemoteUrl,
  parseGitConfig,
  resolveProject,
} from './git.js';

describe('canonicalizeRemoteUrl', () => {
  it('handles git@host:owner/repo.git', () => {
    assert.equal(
      canonicalizeRemoteUrl('git@github.com:AgentWorkforce/burn.git'),
      'github.com/AgentWorkforce/burn',
    );
  });

  it('handles https://host/owner/repo.git', () => {
    assert.equal(
      canonicalizeRemoteUrl('https://github.com/AgentWorkforce/burn.git'),
      'github.com/AgentWorkforce/burn',
    );
  });

  it('handles https without .git suffix', () => {
    assert.equal(
      canonicalizeRemoteUrl('https://gitlab.com/group/sub/repo'),
      'gitlab.com/group/sub/repo',
    );
  });

  it('handles user@ in https url', () => {
    assert.equal(
      canonicalizeRemoteUrl('https://user:token@github.com/foo/bar.git'),
      'github.com/foo/bar',
    );
  });

  it('handles ssh://user@host:port/path', () => {
    assert.equal(
      canonicalizeRemoteUrl('ssh://git@github.com:22/AgentWorkforce/burn.git'),
      'github.com/AgentWorkforce/burn',
    );
  });

  it('lowercases host but preserves owner/repo case', () => {
    assert.equal(
      canonicalizeRemoteUrl('git@GitHub.COM:AgentWorkforce/Burn.git'),
      'github.com/AgentWorkforce/Burn',
    );
  });

  it('returns undefined on junk', () => {
    assert.equal(canonicalizeRemoteUrl(''), undefined);
    assert.equal(canonicalizeRemoteUrl('not a url'), undefined);
    assert.equal(canonicalizeRemoteUrl('https://example.com/'), undefined);
  });

  it('strips trailing slashes', () => {
    assert.equal(
      canonicalizeRemoteUrl('https://github.com/foo/bar/'),
      'github.com/foo/bar',
    );
  });
});

describe('parseGitConfig', () => {
  it('parses simple sections', () => {
    const cfg = parseGitConfig(`
[core]
	repositoryformatversion = 0
[remote "origin"]
	url = git@github.com:foo/bar.git
	fetch = +refs/heads/*:refs/remotes/origin/*
`);
    assert.equal(cfg['core']?.['repositoryformatversion'], '0');
    assert.equal(cfg['remote "origin"']?.['url'], 'git@github.com:foo/bar.git');
  });

  it('ignores comments and blank lines', () => {
    const cfg = parseGitConfig(`
# a comment
; another comment
[remote "origin"]
	url = https://github.com/foo/bar ; inline comment
`);
    assert.equal(cfg['remote "origin"']?.['url'], 'https://github.com/foo/bar');
  });
});

describe('resolveProject', () => {
  let root: string;

  beforeEach(async () => {
    __resetResolveProjectCacheForTesting();
    root = await mkdtemp(path.join(tmpdir(), 'burn-git-test-'));
  });

  afterEach(async () => {
    await rm(root, { recursive: true, force: true });
  });

  it('returns {project: cwd} when no .git exists', () => {
    const got = resolveProject(root);
    assert.equal(got.project, root);
    assert.equal(got.projectKey, undefined);
  });

  it('resolves projectKey from a .git directory', async () => {
    const gitDir = path.join(root, '.git');
    await mkdir(gitDir, { recursive: true });
    await writeFile(
      path.join(gitDir, 'config'),
      '[remote "origin"]\n\turl = git@github.com:foo/bar.git\n',
      'utf8',
    );
    const nested = path.join(root, 'packages', 'a');
    await mkdir(nested, { recursive: true });
    const got = resolveProject(nested);
    assert.equal(got.project, nested);
    assert.equal(got.projectKey, 'github.com/foo/bar');
  });

  it('resolves projectKey from a worktree (.git file + commondir)', async () => {
    const commonGit = path.join(root, 'main', '.git');
    await mkdir(commonGit, { recursive: true });
    await writeFile(
      path.join(commonGit, 'config'),
      '[remote "origin"]\n\turl = https://github.com/foo/bar\n',
      'utf8',
    );
    const worktreeDir = path.join(commonGit, 'worktrees', 'branch-a');
    await mkdir(worktreeDir, { recursive: true });
    await writeFile(path.join(worktreeDir, 'commondir'), '../..\n', 'utf8');

    const worktree = path.join(root, 'worktree-a');
    await mkdir(worktree, { recursive: true });
    await writeFile(path.join(worktree, '.git'), `gitdir: ${worktreeDir}\n`, 'utf8');

    const got = resolveProject(worktree);
    assert.equal(got.project, worktree);
    assert.equal(got.projectKey, 'github.com/foo/bar');
  });

  it('memoizes by cwd', async () => {
    const gitDir = path.join(root, '.git');
    await mkdir(gitDir, { recursive: true });
    await writeFile(
      path.join(gitDir, 'config'),
      '[remote "origin"]\n\turl = git@github.com:foo/bar.git\n',
      'utf8',
    );
    const a = resolveProject(root);
    await writeFile(
      path.join(gitDir, 'config'),
      '[remote "origin"]\n\turl = git@github.com:zzz/zzz.git\n',
      'utf8',
    );
    const b = resolveProject(root);
    assert.equal(a.projectKey, b.projectKey, 'memo returns first result');
  });
});
