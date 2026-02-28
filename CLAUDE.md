# kbtz

## Workflow

All changes must be submitted as pull requests. Never merge directly to main without a PR.

## Plugin versioning

When modifying any file under `plugin/`, bump the plugin version in **both**:

1. `plugin/.claude-plugin/plugin.json` — the `"version"` field
2. `.claude-plugin/marketplace.json` — the `"version"` field for the `kbtz-tools` entry

Both files must have the same version. Use semver (`MAJOR.MINOR.PATCH`): bump PATCH for fixes, MINOR for new features or non-breaking changes, MAJOR for breaking changes.
