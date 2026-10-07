//! Built-in source parsers. Each one registers itself here; the importer only
//! ever sees the [`Registry`].

use crate::parser::Registry;

pub mod antigravity;
pub mod chatgpt;
pub mod claude_code;
pub mod claude_web;
pub mod codex;
pub mod deepseek;
pub mod gemini_cli;
pub mod gemini_web;
pub mod generic;
pub mod hermes;
mod jsonl;
pub mod minimax;
pub mod opencode;
pub mod perplexity;
pub mod pi;
mod rows;
mod webexport;

/// Register every parser in this module. Order is the tie-break order of
/// [`Registry`]: most specific first.
pub fn register_all(registry: &mut Registry) {
    registry.register(Box::new(claude_code::ClaudeCodeParser));
    registry.register(Box::new(codex::CodexParser));
    registry.register(Box::new(chatgpt::ChatGptParser));
    registry.register(Box::new(claude_web::ClaudeWebParser));
    registry.register(Box::new(gemini_web::GeminiWebParser));
    registry.register(Box::new(perplexity::PerplexityParser));
    registry.register(Box::new(opencode::OpenCodeParser));
    registry.register(Box::new(pi::PiParser::pi()));
    registry.register(Box::new(pi::PiParser::oh_my_pi()));
    registry.register(Box::new(hermes::HermesParser));
    registry.register(Box::new(gemini_cli::GeminiCliParser));
    registry.register(Box::new(antigravity::AntigravityParser));
    registry.register(Box::new(minimax::MiniMaxParser));
    registry.register(Box::new(deepseek::DeepSeekParser));
    registry.register(Box::new(generic::GenericJsonlParser));
    registry.register(Box::new(generic::GenericJsonParser));
}

/// A registry holding every built-in parser.
pub fn registry() -> Registry {
    let mut r = Registry::new(Vec::new());
    register_all(&mut r);
    r
}

/// Why a file inside a known app store is inventoried as unsupported rather than
/// imported: `(format label, reason)`. Consulted only after no parser claimed it.
pub fn known_unsupported(probe: &crate::source::Probe) -> Option<(&'static str, &'static str)> {
    sqlite_companion(probe)
        .or_else(|| claude_code::known_unsupported(probe))
        .or_else(|| antigravity::known_unsupported(probe))
        .or_else(|| minimax::known_unsupported(probe))
        .or_else(|| deepseek::known_unsupported(probe))
        .or_else(|| desktop_installation(probe))
        .or_else(|| webexport::known_unsupported(probe))
}

fn desktop_installation(probe: &crate::source::Probe) -> Option<(&'static str, &'static str)> {
    let components: Vec<_> = std::path::Path::new(&probe.full_path)
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    components
        .windows(2)
        .any(|p| p[0] == "AnthropicClaude" && (p[1].starts_with("app-") || p[1] == "packages"))
        .then_some((
            "desktop-installation-file",
            "Claude desktop installation/runtime/package asset; no supported conversation history",
        ))
}

/// `-wal` / `-shm` / `-journal` next to a database: collected so the database
/// reads consistently, but never a source of its own.
fn sqlite_companion(probe: &crate::source::Probe) -> Option<(&'static str, &'static str)> {
    let name = probe.filename();
    ["-wal", "-shm", "-journal"]
        .iter()
        .any(|s| name.strip_suffix(s).is_some_and(crate::discover::is_sqlite_name))
        .then_some((
            "sqlite-companion",
            "write-ahead/shared-memory/journal sidecar of a SQLite database; read together with the main database file, not parsed on its own",
        ))
}
