# Release dry run

Shows the release PRs release-please would open for a checkout, with every
file change, without touching GitHub. It runs release-please's own `Manifest`
on a local clone (`LocalGitHub`); only the GitHub releases list is given by
hand.

```bash
mkdir -p /tmp/rp && (cd /tmp/rp && npm init -y >/dev/null && npm install release-please@17)
cp tools/release-dry-run/dry-run.js /tmp/rp/
# A checkout whose `main` is what you want to check (merge a branch into it first):
node /tmp/rp/dry-run.js "$PWD" '[{"tagName":"v0.6.0","sha":"<release commit>","name":"v0.6.0","notes":""}]'
```

The second argument lists the GitHub releases that exist (newest first):
`tagName` and the `sha` of the commit the release points at. Use it after
changing `release-please-config.json`: Vigilant Vole's PR should only touch
`CHANGELOG.md`, `Cargo.toml`, the `vigilant-vole` entry in `Cargo.lock` and
the manifest; vole-kit's only `kit/`, the `vole-kit` and `vole-kit-build`
entries in `Cargo.lock` and the manifest. See docs/vole-kit.md, "Releases".
