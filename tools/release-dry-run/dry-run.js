// Dry run of release-please's real Manifest on a local checkout: files,
// commits and tags from git; the GitHub releases list from the command line.
// See README.md.
const {LocalGitHub} = require('release-please/build/src/local-github');
const {Manifest} = require('release-please/build/src/manifest');
const dir = process.argv[2];
const releases = JSON.parse(process.argv[3] || '[]');
const repository = {owner: 'Ferdinand99', repo: 'Vigilant-Vole', defaultBranch: 'main'};
const api = {
  repository,
  async *releaseIterator() { for (const r of releases) yield r; },
  async *pullRequestIterator() {},
};
const quiet = {error(){}, warn(){}, info(){}, debug(){}, trace(){}};
(async () => {
  const gh = new LocalGitHub(repository, api, dir, {logger: quiet});
  const m = await Manifest.fromManifest(gh, 'main', 'release-please-config.json', '.release-please-manifest.json', {logger: quiet});
  const prs = await m.buildPullRequests();
  for (const pr of prs) {
    console.log('=== PR:', pr.title.toString(), '| branch', pr.headRefName, '| version', pr.version && pr.version.toString());
    for (const u of pr.updates) {
      let before = null;
      try { before = (await gh.getFileContentsOnBranch(u.path, 'main')).parsedContent; } catch (e) {}
      if (before === null && !u.createIfMissing) { console.log('  skip (missing):', u.path); continue; }
      const after = u.updater.updateContent(before === null ? undefined : before);
      const b = (before || '').split('\n'), a = after.split('\n');
      const changed = [];
      for (let i = 0; i < Math.max(a.length, b.length); i++) if (a[i] !== b[i]) changed.push(`    ${i+1}: ${JSON.stringify(b[i])} -> ${JSON.stringify(a[i])}`);
      console.log(`  ${u.path}: ${changed.length} changed line(s)` + (u.path.endsWith('CHANGELOG.md') ? '' : '\n' + changed.slice(0, 6).join('\n')));
      if (u.path.endsWith('CHANGELOG.md')) console.log(after.split('\n').slice(0, 14).map(l => '    | ' + l).join('\n'));
    }
  }
})().catch(e => { console.error(e); process.exit(1); });
