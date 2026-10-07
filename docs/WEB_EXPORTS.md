# Official web-chat exports

`convolith import ~/Downloads/chatgpt-export.zip -o ./canonical-ai-history` (a zip, an
extracted folder, or the bare `conversations.json` / Takeout file) auto-detects the
provider. Only the files the providers hand out are read: no browser scraping, no
network. Zips go through the usual expansion limits (path traversal, zip-bomb ratios,
size caps). Provenance records the zip path plus the member path and the importing
machine. Re-importing the same or a newer export is idempotent: ids come from the
provider's conversation/message ids, so only new or changed events are added. Events
from these exports are only merged with a local source on an identical provider id
under the same provider+application; identical text alone never merges.

| Export | Parser | Status | How to request it |
|--------|--------|--------|-------------------|
| Claude (claude.ai) data export: `conversations-NNN.zip` > `conversations.json` (with the sibling `manifest-*.json` and part zips) | `claude_web_export` | **verified** against a real export (schema only; synthetic-content tests) | claude.ai: Settings > Privacy > Export data; the link arrives by email |
| Claude Design chats: `design_chats-NNN.zip` > `design_chats/<uuid>.json` | `claude_design_export` | **verified** against a real export | same Claude export |
| Perplexity user data export: `user_data_export_*.zip` > `conversations-*.json` (+ `user-data-*.xlsx`) | `perplexity_export` | **verified** against two real exports | Perplexity: Settings > Account > Download/Export your data |
| Gemini via Google Takeout, `Gemini in Workspace/Conversation History/conversation_*.txt` | `gemini_takeout` | **verified** against a real Takeout | takeout.google.com: select Gemini / Gemini in Workspace |
| Gemini via Google Takeout, `My Activity/Gemini Apps/MyActivity.json` | `gemini_takeout` | **unverified-against-real-export** (none available; the real Takeout had no My Activity folder) | takeout.google.com: My Activity > Gemini Apps, format JSON |
| ChatGPT data export (`conversations.json`; sharded `conversations-NNN.json` handled but unseen) | `chatgpt_export` | **verified** against a real export (84 conversations; schema only, synthetic-content tests); v0.1.0 event/conversation ids preserved | ChatGPT: Settings > Data controls > Export data; the link arrives by email |

Not guessed at: an export whose schema or declared version is not recognised is listed
in the import report as `unsupported` with the reason, never parsed on a best guess.
Also inventoried (not imported), each with its reason: ChatGPT `chat.html`, `user.json`,
`message_feedback.json`, `ads.json`, `user_settings.json`, `library_files.json`, manifests and
indexes, and the `file_*.dat` attachment/image bytes (named per message in `chatgpt_asset_members`); Claude `manifest-*.json`, `users.json`,
`login_history.json`, `memories/`, `projects/` (project docs), `reflections/` (feedback),
`artifacts/` (design frames); Perplexity `user-data-*.xlsx`; Takeout NotebookLM, Gems and
`MyActivity.html`; conversation images; Office-document internals. Attachments are kept as references (name, mime,
provider file id); their bytes are not imported. Gemini's per-turn times are
*last-modified* times and are marked so (`gemini_timestamp_kind`); a Takeout turn is either a
user or a model turn and `turn_index` can repeat, so turns are identified by role, index and
occurrence. Claude exports carry real edit/retry branches (`parent_message_uuid`; the
all-zero root sentinel means no parent). ChatGPT branches (regenerations) are all kept, recovered from `parent` (real exports
have no `children`); `chatgpt_on_current_path` marks the live one. ChatGPT `thoughts` and
`reasoning_recap` messages are kept in event metadata (`chatgpt_content`), not as content
parts, so v0.1.0 content fingerprints stay valid. Messages copied by conversation branching
share ids across conversations and merge into one event with several observations.
