//! Secret detection and redaction policy.
//!
//! Originals are never modified. The policy only affects the *canonical,
//! searchable* representation and normal log output: a redacted span is
//! replaced by a stable marker, and the fact that a redaction happened is
//! recorded per event (`redactions[]`) and aggregated in
//! `reports/PRIVACY_AUDIT.md`. With `--secret-policy preserve` the canonical
//! data keeps the original text and the audit report still counts what was
//! found, so the operator can see the exposure without it leaking into logs.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretPolicy {
    Redact,
    Preserve,
}

impl SecretPolicy {
    pub fn parse(s: &str) -> Option<SecretPolicy> {
        match s.to_ascii_lowercase().as_str() {
            "redact" | "redacted" => Some(SecretPolicy::Redact),
            "preserve" | "keep" => Some(SecretPolicy::Preserve),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionHit {
    pub kind: String,
    pub count: u32,
    pub field: String,
}

struct Pattern {
    kind: &'static str,
    re: Regex,
}

fn patterns() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| {
        let mk = |kind: &'static str, pat: &str| Pattern { kind, re: Regex::new(pat).unwrap() };
        vec![
            // The whole PEM block, not just its header: the base64 body is the
            // secret. An unterminated block (truncated output) takes the header
            // plus the base64-looking run that follows.
            mk(
                "private_key",
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----(?:.*?-----END [A-Z ]*PRIVATE KEY-----|[A-Za-z0-9+/=\s]{0,8192})",
            ),
            // Specific `sk-` shapes first: the generic OpenAI pattern also
            // matches them, and a secret is counted under its most specific kind.
            mk("anthropic_key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b"),
            mk("openrouter_key", r"\bsk-or-v1-[A-Za-z0-9]{20,}\b"),
            mk(
                "openai_key",
                r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}\b",
            ),
            mk("github_token", r"\bgh[pousr]_[A-Za-z0-9]{20,}\b"),
            mk("github_pat", r"\bgithub_pat_[A-Za-z0-9_]{20,}\b"),
            mk("slack_token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}\b"),
            mk("google_api_key", r"\bAIza[0-9A-Za-z_\-]{30,}\b"),
            mk("aws_access_key_id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            mk(
                "aws_secret_access_key",
                r#"(?i)aws_?secret_?access_?key["'\s:=]{1,4}([A-Za-z0-9/+=]{40})"#,
            ),
            mk("huggingface_token", r"\bhf_[A-Za-z0-9]{20,}\b"),
            mk("minimax_key", r"\bsk-[A-Za-z0-9]{40,}\b"),
            mk("jwt", r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{5,}\b"),
            mk(
                "authorization_header",
                r#"(?i)\bauthorization\s*[:=]\s*(?:bearer|basic|token)\s+[A-Za-z0-9._\-+/=]{12,}"#,
            ),
            mk(
                "cookie_header",
                r#"(?i)\b(?:set-)?cookie\s*[:=]\s*[A-Za-z0-9_\-\.]{2,}=[^;\s'"]{8,}"#,
            ),
            mk(
                "password_assignment",
                r#"(?i)\b(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret)\b\s*[:=]\s*["']?[^\s"',;]{8,}"#,
                // A `[^...]` class that *excludes* a quote must still be able
                // to sit inside a `r#"..."#` literal; the two patterns above
                // are the ones that tripped over the closing delimiter.
            ),
            mk("basic_auth_url", r"\b[a-z][a-z0-9+.\-]{2,8}://[^\s/:@]{1,64}:[^\s/@]{6,}@"),
        ]
    })
}

#[derive(Clone)]
pub struct Secrets {
    policy: SecretPolicy,
}

impl Secrets {
    pub fn new(policy: SecretPolicy) -> Secrets {
        Secrets { policy }
    }

    /// A handle usable from another thread. `Secrets` holds only a policy enum
    /// and reaches its compiled patterns through a `OnceLock`, so this is a
    /// plain clone rather than anything unsafe.
    pub fn clone_for_thread(&self) -> Secrets {
        self.clone()
    }

    /// Apply the policy to every string inside a JSON value, reporting the
    /// aggregate. Tool arguments and tool output carry credentials as often as
    /// prose does, so they must not bypass the policy.
    pub fn apply_json(
        &self,
        field: &str,
        value: &serde_json::Value,
    ) -> (serde_json::Value, Option<RedactionHit>) {
        let (v, hits) = self.apply_json_detailed(field, value);
        (v, collapse(field, hits))
    }

    /// Like [`apply_json`](Self::apply_json) with one hit per secret kind.
    pub fn apply_json_detailed(
        &self,
        field: &str,
        value: &serde_json::Value,
    ) -> (serde_json::Value, Vec<RedactionHit>) {
        use serde_json::Value;
        let mut hits: Vec<RedactionHit> = Vec::new();
        let mut walk = |v: &Value, this: &Secrets| -> Value {
            let (nv, h) = this.apply_json_detailed(field, v);
            merge_hits(&mut hits, h);
            nv
        };
        let out = match value {
            Value::String(s) => {
                let (t, h) = self.apply_detailed(field, s);
                merge_hits(&mut hits, h);
                Value::String(t)
            }
            Value::Array(a) => Value::Array(a.iter().map(|v| walk(v, self)).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, v)| (k.clone(), walk(v, self))).collect())
            }
            other => other.clone(),
        };
        (out, hits)
    }

    pub fn policy(&self) -> SecretPolicy {
        self.policy
    }

    /// Detect secrets in `text`. Always returns the findings, regardless of
    /// policy, so the privacy audit can report exposure truthfully.
    pub fn detect(&self, text: &str) -> Vec<(&'static str, usize)> {
        let mut out = Vec::new();
        for p in patterns() {
            let n = p.re.find_iter(text).count();
            if n > 0 {
                out.push((p.kind, n));
            }
        }
        out
    }

    /// Apply the policy. Returns the text to store and an aggregate hit.
    pub fn apply(&self, field: &str, text: &str) -> (String, Option<RedactionHit>) {
        let (t, hits) = self.apply_detailed(field, text);
        (t, collapse(field, hits))
    }

    /// Apply the policy, reporting one hit per secret kind (counts only).
    pub fn apply_detailed(&self, field: &str, text: &str) -> (String, Vec<RedactionHit>) {
        let mut hits = Vec::new();
        let mut redacted: Option<String> = None;
        for p in patterns() {
            let current = redacted.as_deref().unwrap_or(text);
            let n = p.re.find_iter(current).count() as u32;
            if n == 0 {
                continue;
            }
            hits.push(RedactionHit {
                kind: p.kind.to_string(),
                count: n,
                field: field.to_string(),
            });
            if self.policy == SecretPolicy::Redact {
                let marker = format!("[REDACTED:{}]", p.kind);
                redacted = Some(p.re.replace_all(current, marker.as_str()).into_owned());
            }
        }
        (redacted.unwrap_or_else(|| text.to_string()), hits)
    }
}

fn merge_hits(into: &mut Vec<RedactionHit>, from: Vec<RedactionHit>) {
    for h in from {
        match into.iter_mut().find(|e| e.kind == h.kind) {
            Some(e) => e.count += h.count,
            None => into.push(h),
        }
    }
}

fn collapse(field: &str, hits: Vec<RedactionHit>) -> Option<RedactionHit> {
    let total: u32 = hits.iter().map(|h| h.count).sum();
    if total == 0 {
        return None;
    }
    let kind = match hits.as_slice() {
        [one] => one.kind.clone(),
        many => format!(
            "multiple({})",
            many.iter()
                .map(|h| h.kind.as_str())
                .collect::<Vec<_>>()
                .join("+")
        ),
    };
    Some(RedactionHit {
        kind,
        count: total,
        field: field.to_string(),
    })
}

/// Redact secrets from a string that is about to be logged or reported.
/// Logs always redact, independent of `--secret-policy`.
pub fn scrub_for_log(text: &str) -> String {
    let s = Secrets::new(SecretPolicy::Redact);
    s.apply("log", text).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_redacts_common_keys() {
        let s = Secrets::new(SecretPolicy::Redact);
        let (out, hit) = s.apply(
            "content[0].text",
            "key=sk-abcdefghijklmnopqrstuvwxyz012345 token",
        );
        assert!(out.contains("[REDACTED:openai_key]"));
        assert!(!out.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
        assert!(hit.is_some());

        let (out, _) = s.apply("x", "Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
        assert!(out.contains("REDACTED"), "{out}");
    }

    #[test]
    fn preserve_policy_keeps_text_but_reports() {
        let s = Secrets::new(SecretPolicy::Preserve);
        let text = "AKIAIOSFODNN7EXAMPLE";
        let (out, hit) = s.apply("x", text);
        assert_eq!(out, text);
        assert!(hit.is_some());
    }

    #[test]
    fn private_key_marker() {
        let s = Secrets::new(SecretPolicy::Redact);
        let (out, _) = s.apply("x", "-----BEGIN OPENSSH PRIVATE KEY-----");
        assert_eq!(out, "[REDACTED:private_key]");
    }

    #[test]
    fn clean_text_untouched() {
        let s = Secrets::new(SecretPolicy::Redact);
        let text = "hello world, nothing secret here";
        assert_eq!(s.apply("x", text).0, text);
        assert!(s.apply("x", text).1.is_none());
    }
}
