# Hook fixtures

These are **synthetic** payloads modeled on the confirmed shapes from SPEC §3.5.

The only payload whose field set is verified end-to-end against a real Claude session is
`claude-user-prompt-submit.json` (`session_id` / `transcript_path` / `cwd` / `permission_mode` /
`hook_event_name` / `prompt`). The rest are best-guess based on Claude/Codex documentation
plus the Codezilla `pre-tool-use.sh` extraction rules that already work in production.

**Action required during M7 dogfood:** replace every fixture in this directory with a
real, sanitized capture taken by running Claude / Codex with `bash -x` traces or by adding
a temporary `cat - > /tmp/heed-hook-fixture.json` line to the relevant hook script.

When replacing, preserve field names and nesting exactly; sanitize prompts, absolute paths
that reveal local context, and any auth identifiers.
