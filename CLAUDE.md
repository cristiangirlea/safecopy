@AGENTS.md

## Claude Code

- Run `/check` before every push; it runs what CI runs.
- `.claude/settings.json` sets no attribution; don't add `Co-Authored-By`
  or "Generated with" lines by hand.
- `gh pr merge` asks first; never `--auto` or `--admin`.
- Personal overrides go in `CLAUDE.local.md` and
  `.claude/settings.local.json`, both ignored by git.
