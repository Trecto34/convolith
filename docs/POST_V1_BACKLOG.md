# Post-v1 backlog

This list captures follow-up engineering work. Validation should use synthetic
fixtures by default. If live stores are used for local validation, keep their
contents, identifiers, machine names, paths, credentials, and logs outside the
repository and report only what is necessary.

## Follow-up items

1. **Machine event counts.** Compare the aggregate `machine.event_count` values
   with indexed event counts and define a migration or repair if they diverge.
2. **Legacy resume state.** Older state keys may trigger one full re-import after
   an identity format change. Preserve idempotence and avoid unnecessary work.
3. **Remote resume transfer.** Consider a remote size/mtime manifest so unchanged
   files can be skipped before transferring their contents.
4. **Antigravity history.** Undocumented trajectory databases and journal entries
   remain unsupported unless their schema, message identity, ordering, and
   duplicate behavior can be established safely. See
   [storage audit](ANTIGRAVITY_STORAGE_AUDIT.md).

## Validation scope

Platform-specific behavior should be validated on the target platform. Keep
reports in the repository limited to reproducible commands, synthetic examples,
and non-sensitive outcomes.
