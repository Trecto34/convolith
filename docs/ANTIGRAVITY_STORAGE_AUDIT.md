# Antigravity storage audit

Antigravity CLI data can include transcript files, SQLite databases, protobuf
trajectory payloads, and a history journal. Some databases have no matching
transcript; empty trajectories add no events, while nonempty protobuf payloads
remain unsupported until their schema and relationship to transcripts can be
validated.

The history journal has no per-message native id. Similar text may recur, and
command records are not necessarily model conversations. Do not import journal
records unless a stable cross-source identity and position mapping prevents
duplicates.

The parser intentionally imports supported transcript files only. No database
parser is added for undocumented trajectory payloads, and no content from live
stores is included in this repository.
