//! Canonical identity and the deduplication decision.
//!
//! Deduplicating a personal archive is asymmetric: leaving a duplicate behind
//! costs disk, while a false merge *loses* history and rewrites provenance.
//! Every rule here is therefore biased towards keeping records apart, and the
//! tier that was used is recorded on the event and in the ledger so a reviewer
//! can audit any collapse after the fact.
//!
//! Identity tiers, strongest first ([`crate::parser::IdentityHint`]):
//!
//! 1. **native** — a provider id that is globally unique on its own (a UUID, or
//!    a long opaque token). The conversation does not have to be known: two
//!    snapshots of the same store then collapse even if one of them lost its
//!    session header.
//! 2. **coordinates** — a provider id that is only unique *within* its
//!    conversation (`"msg_1"`), or no id at all but a stable source position.
//!    The key includes the conversation, so the same id in another thread can
//!    never merge.
//! 3. **fingerprint** — nothing stable: conversation + sequence + role + a
//!    content fingerprint. This only collapses records that agree on position
//!    *and* content, so it cannot merge two different messages.
//!
//! Content is never hashed alone at any tier.

use crate::id::{hash_parts, hex};
use crate::model::{compact_json, Event, Part};
use crate::parser::IdentityHint;
use std::path::Path;

/// True when an id is unique without knowing which conversation it came from.
///
/// Deliberately conservative: a bare `"1"` or `"msg_3"` is *not* treated as
/// global, because providers reuse those inside a thread and merging on them
/// would fuse unrelated conversations. A UUID or a long random-looking token is.
pub fn is_globally_unique(native_id: &str) -> bool {
    let s = native_id.trim();
    if s.len() < 16 {
        return false;
    }
    // A UUID in any of the usual spellings.
    if looks_like_uuid(s) {
        return true;
    }
    // Otherwise require real length *and* mixed alphabet, so `0000000000000000000`
    // is not mistaken for a unique token.
    let has_alpha = s.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    let distinct = {
        let mut seen = [false; 128];
        for b in s.bytes() {
            if b < 128 {
                seen[b as usize] = true;
            }
        }
        seen.iter().filter(|x| **x).count()
    };
    s.len() >= 20 && (has_alpha && has_digit) && distinct >= 12
}

fn looks_like_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let hex = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_hexdigit());
    match parts.len() {
        // 8-4-4-4-12, the canonical spelling.
        5 => {
            const LENS: [usize; 5] = [8, 4, 4, 4, 12];
            parts.iter().zip(LENS).all(|(p, n)| p.len() == n && hex(p))
        }
        // `<prefix>-<32+ hex>`, e.g. `chatcmpl-8f14e45f...`. Any part of that
        // shape is enough: the length is what makes it unique, not the prefix.
        _ => parts.iter().any(|p| p.len() >= 32 && hex(p)),
    }
}

/// Everything needed to derive a canonical id for one event.
pub struct IdentityInput<'a> {
    pub provider: &'a str,
    pub application: &'a str,
    /// Stable identity of the conversation: a provider id, or a deterministic
    /// coordinate key when the provider has none.
    pub conversation_key: &'a str,
    pub native_id: Option<&'a str>,
    pub seq: u64,
    pub role: &'a str,
    pub event_type: &'a str,
    pub hint: IdentityHint,
}

pub struct Identity {
    pub event_id: String,
    pub tier: &'static str,
}

/// Derive the canonical event id and the tier that produced it.
pub fn event_identity(input: &IdentityInput<'_>, content_fp: &str) -> Identity {
    let native = input.native_id.map(str::trim).filter(|s| !s.is_empty());
    match native {
        Some(n) if is_globally_unique(n) => Identity {
            event_id: crate::id::id("ev_", &[input.provider, input.application, "native", n]),
            tier: "native",
        },
        Some(n) => Identity {
            // Provider-local id: safe only together with the conversation.
            event_id: crate::id::id(
                "ev_",
                &[
                    input.provider,
                    input.application,
                    "coord",
                    input.conversation_key,
                    n,
                ],
            ),
            tier: "coordinates",
        },
        None if input.hint == IdentityHint::Coordinates => Identity {
            event_id: crate::id::id(
                "ev_",
                &[
                    input.provider,
                    input.application,
                    "pos",
                    input.conversation_key,
                    &input.seq.to_string(),
                    input.role,
                    input.event_type,
                ],
            ),
            tier: "coordinates",
        },
        None => Identity {
            event_id: crate::id::id(
                "ev_",
                &[
                    input.provider,
                    input.application,
                    "fp",
                    input.conversation_key,
                    &input.seq.to_string(),
                    input.role,
                    input.event_type,
                    content_fp,
                ],
            ),
            tier: "fingerprint",
        },
    }
}

/// A conversation identity that survives being observed from several sources.
///
/// With no provider id, identity falls back to the supplied source identity
/// components. Callers may use stable source material or content-derived keys.
pub fn conversation_key(
    provider: &str,
    application: &str,
    native_conversation_id: Option<&str>,
    native_session_id: Option<&str>,
    fallback: &[&str],
) -> (String, IdentityHint) {
    if let Some(c) = native_conversation_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if is_globally_unique(c) {
            return (
                crate::id::id("cv_", &[provider, application, "native", c]),
                IdentityHint::Native,
            );
        }
    }
    if let Some(s) = native_session_id.map(str::trim).filter(|s| !s.is_empty()) {
        if is_globally_unique(s) {
            return (
                crate::id::id("cv_", &[provider, application, "native", s]),
                IdentityHint::Native,
            );
        }
    }
    let mut parts: Vec<&str> = vec![provider, application, "coord"];
    parts.extend_from_slice(fallback);
    (crate::id::id("cv_", &parts), IdentityHint::Coordinates)
}

pub fn session_id_for(provider: &str, application: &str, native_session_id: &str) -> String {
    crate::id::id("se_", &[provider, application, native_session_id])
}

pub fn project_id_for(name: &str) -> String {
    crate::id::id("prj_", &["name", name])
}

/// Fingerprint of the *stored* content of an event, computed after secret
/// policy and truncation so that two runs agree even when the policy differs
/// from the source bytes.
pub fn content_fingerprint(event: &Event) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(event.content.len());
    for p in &event.content {
        parts.push(match p {
            Part::Text { text, truncation } => match truncation {
                Some(t) => format!("t:{}:{}", t.full_sha256, t.inline_bytes),
                None => format!("t:{text}"),
            },
            Part::Reasoning { text, visibility } => format!("r:{visibility:?}:{text}"),
            Part::ToolCall {
                id,
                name,
                arguments,
            } => {
                format!(
                    "c:{}:{name}:{}",
                    id.clone().unwrap_or_default(),
                    compact_json(arguments)
                )
            }
            Part::ToolResult {
                tool_call_id,
                output,
                is_error,
            } => format!(
                "o:{}:{is_error}:{}",
                tool_call_id.clone().unwrap_or_default(),
                compact_json(output)
            ),
            Part::Image {
                artifact,
                source_ref,
                mime,
                filename,
            } => format!(
                "i:{}:{}:{}:{}",
                artifact.clone().unwrap_or_default(),
                source_ref.clone().unwrap_or_default(),
                mime.clone().unwrap_or_default(),
                filename.clone().unwrap_or_default()
            ),
            Part::FileRef {
                path,
                artifact,
                mime,
                filename,
            } => format!(
                "f:{}:{}:{}:{}",
                path.clone().unwrap_or_default(),
                artifact.clone().unwrap_or_default(),
                mime.clone().unwrap_or_default(),
                filename.clone().unwrap_or_default()
            ),
            Part::Artifact {
                artifact,
                size,
                mime,
                filename,
            } => format!(
                "a:{artifact}:{size}:{}:{}",
                mime.clone().unwrap_or_default(),
                filename.clone().unwrap_or_default()
            ),
            Part::Data { value } => format!("d:{}", compact_json(value)),
            Part::Opaque { kind, raw, note } => format!(
                "x:{kind}:{}:{}",
                note.clone().unwrap_or_default(),
                raw.as_ref().map(compact_json).unwrap_or_default()
            ),
        });
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    hex(&hash_parts(&refs))
}

/// Stable identity for a source record coordinate, used in the ledger and for
/// the coordinate tier when a parser cannot name its conversation.
pub fn source_identity(display_path: &str, container_chain: &[String]) -> String {
    let mut parts: Vec<&str> = vec![display_path];
    parts.extend(container_chain.iter().map(String::as_str));
    crate::id::id("src_", &parts)
}

/// Human-stable id for an import run.
pub fn import_run_id(started_at: &str) -> String {
    let h = hex(&hash_parts(&[started_at, &std::process::id().to_string()]));
    format!("run_{}", &h[..12])
}

/// A filesystem fingerprint used by `--resume`. Includes size and mtime so a
/// changed source is always re-examined.
pub fn file_fingerprint(path: &Path) -> String {
    let md = std::fs::metadata(path);
    match md {
        Ok(m) => {
            let size = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(crate::timeutil::from_system_time)
                .map(|u| u.0)
                .unwrap_or(0);
            let name = path.to_string_lossy().to_string();
            let mut parts = vec![name.as_str()];
            let size_s = size.to_string();
            let mtime_s = mtime.to_string();
            parts.push(&size_s);
            parts.push(&mtime_s);
            crate::id::id("ff_", &parts)
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventType, Role};

    fn uuids_are_global() {
        assert!(is_globally_unique("9a1f4c2e-3b6d-4e8a-9c1f-2d5b7a8e0f31"));
        assert!(is_globally_unique("01J8Z5K2M3N4P5Q6R7S8T9V0W1"));
        assert!(is_globally_unique(
            "chatcmpl-8f14e45fceea167a5a36dedd4bea2543"
        ));
    }

    #[test]
    fn short_or_repeating_ids_are_not_global() {
        uuids_are_global();
        assert!(!is_globally_unique("1"));
        assert!(!is_globally_unique("msg_3"));
        assert!(!is_globally_unique("00000000000000000000000"));
        assert!(!is_globally_unique("aaaaaaaaaaaaaaaaaaaa"));
        assert!(!is_globally_unique(""));
    }

    #[test]
    fn native_ids_merge_across_conversations_but_local_ones_do_not() {
        let global = IdentityInput {
            provider: "openai",
            application: "chatgpt",
            conversation_key: "cv_A",
            native_id: Some("9a1f4c2e-3b6d-4e8a-9c1f-2d5b7a8e0f31"),
            seq: 3,
            role: "user",
            event_type: "message",
            hint: IdentityHint::Native,
        };
        let a = event_identity(&global, "fp1");
        let mut other_conversation = IdentityInput {
            conversation_key: "cv_B",
            ..clone_input(&global)
        };
        let b = event_identity(&other_conversation, "fp1");
        assert_eq!(
            a.event_id, b.event_id,
            "a globally unique id identifies the same event anywhere"
        );
        assert_eq!(a.tier, "native");

        other_conversation.native_id = Some("msg_3");
        let c = event_identity(&other_conversation, "fp1");
        let d = event_identity(&global, "fp1");
        assert_ne!(
            c.event_id, d.event_id,
            "a reused local id must stay conversation-scoped"
        );
        assert_eq!(c.tier, "coordinates");
    }

    fn clone_input<'a>(i: &IdentityInput<'a>) -> IdentityInput<'a> {
        IdentityInput {
            provider: i.provider,
            application: i.application,
            conversation_key: i.conversation_key,
            native_id: i.native_id,
            seq: i.seq,
            role: i.role,
            event_type: i.event_type,
            hint: i.hint,
        }
    }

    #[test]
    fn fingerprint_tier_separates_position_and_content() {
        let base = IdentityInput {
            provider: "p",
            application: "a",
            conversation_key: "cv_A",
            native_id: None,
            seq: 0,
            role: "user",
            event_type: "message",
            hint: IdentityHint::Fingerprint,
        };
        let a = event_identity(&base, "same");
        let same = event_identity(&base, "same");
        let other_content = event_identity(&base, "different");
        let other_seq = event_identity(
            &IdentityInput {
                seq: 1,
                ..clone_input(&base)
            },
            "same",
        );
        assert_eq!(a.event_id, same.event_id);
        assert_ne!(a.event_id, other_content.event_id);
        assert_ne!(a.event_id, other_seq.event_id);
        assert_eq!(a.tier, "fingerprint");
    }

    #[test]
    fn identical_text_in_different_conversations_never_merges() {
        let mk = |cv: &'static str| IdentityInput {
            provider: "p",
            application: "a",
            conversation_key: cv,
            native_id: None,
            seq: 0,
            role: "user",
            event_type: "message",
            hint: IdentityHint::Fingerprint,
        };
        // Two threads that both start with the single word "yes".
        assert_ne!(
            event_identity(&mk("cv_A"), "t:yes").event_id,
            event_identity(&mk("cv_B"), "t:yes").event_id
        );
    }

    #[test]
    fn conversation_key_prefers_native_then_falls_back() {
        let (k1, t1) = conversation_key("p", "a", Some("cv-9a1f4c2e-3b6d-4e8a"), None, &["x"]);
        let (k2, t2) = conversation_key("p", "a", None, Some("cv-9a1f4c2e-3b6d-4e8a"), &["x"]);
        assert_eq!(
            k1, k2,
            "a session id names the conversation when there is no thread id"
        );
        assert_eq!(t1, IdentityHint::Native);
        assert_eq!(t2, IdentityHint::Native);
        let (k3, t3) = conversation_key("p", "a", None, None, &["a.jsonl"]);
        let (k4, _) = conversation_key("p", "a", None, None, &["b.jsonl"]);
        assert_ne!(k3, k4);
        assert_eq!(t3, IdentityHint::Coordinates);
        // A short id is not trusted as a global conversation id.
        let (_, t5) = conversation_key("p", "a", Some("s1"), None, &["f"]);
        assert_eq!(t5, IdentityHint::Coordinates);
    }

    #[test]
    fn fingerprints_ignore_whitespace_only_difference_in_shape_not_bytes() {
        let mk = |text: &str| Event {
            schema_version: 1,
            event_id: "ev_x".into(),
            conversation_id: "cv_x".into(),
            session_id: None,
            parent_event_id: None,
            seq: 0,
            timestamp: None,
            timestamp_original: None,
            timestamp_confidence: crate::timeutil::TimestampConfidence::Unknown,
            role: Role::User,
            event_type: EventType::Message,
            provider: "p".into(),
            application: "a".into(),
            model: None,
            agent: None,
            machine_id: None,
            project_id: None,
            repository_id: None,
            working_directory: None,
            branch: None,
            commit: None,
            worktree_id: None,
            content: vec![Part::text(text)],
            metadata: Default::default(),
            redactions: Vec::new(),
            provenance: Vec::new(),
        };
        assert_eq!(
            content_fingerprint(&mk("hi")),
            content_fingerprint(&mk("hi"))
        );
        assert_ne!(
            content_fingerprint(&mk("hi")),
            content_fingerprint(&mk("hi "))
        );
        assert_ne!(
            content_fingerprint(&mk("hi")),
            content_fingerprint(&mk("HI"))
        );
    }
}
