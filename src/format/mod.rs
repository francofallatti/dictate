//! Optional LLM clean-up of the raw transcript (fillers, punctuation, spoken commands).

use crate::config::Config;
use anyhow::Result;
use std::sync::Arc;

pub mod claude;
pub mod openai;

/// What the user was doing when they started dictating.
#[derive(Debug, Clone, Default)]
pub struct FormatContext {
    pub app_name: String,
    pub window_title: String,
    /// Language code or "auto".
    pub language: String,
}

pub trait Formatter: Send + Sync {
    fn name(&self) -> &'static str;
    fn format(&self, raw: &str, ctx: &FormatContext) -> Result<String>;
}

/// Pass-through formatter.
pub struct NoFormat;

impl Formatter for NoFormat {
    fn name(&self) -> &'static str {
        "none"
    }
    fn format(&self, raw: &str, _ctx: &FormatContext) -> Result<String> {
        Ok(raw.to_string())
    }
}

pub fn build(cfg: &Config) -> Result<Arc<dyn Formatter>> {
    let extra = cfg.format.extra_instructions.clone();
    Ok(match cfg.format.backend.as_str() {
        "none" => Arc::new(NoFormat),
        "claude" => Arc::new(claude::ClaudeFormatter::new(&cfg.format.claude, extra)?),
        "openai" => Arc::new(openai::OpenAiFormatter::new(&cfg.format.openai, extra)?),
        other => anyhow::bail!("unknown format backend {other:?}"),
    })
}

/// Shared instructions for every LLM backend.
pub fn system_prompt(extra: &str) -> String {
    let mut p = String::from(
        "You are the post-processing step of a voice dictation tool. The user spoke, a speech \
recognizer produced the raw transcript, and your job is to turn it into the text the user meant \
to type. Reply with the final text only: no preamble, no quotes, no explanations, no markdown \
fences.\n\n\
Rules:\n\
- Keep the user's language and wording. Do not translate, summarize, answer, or add content.\n\
- Fix punctuation, capitalization and obvious recognition mistakes (homophones, split words).\n\
- Remove filler words, hesitations, repeated words and false starts. When the user corrects \
themselves (\"send it Monday, no, Tuesday\"), keep only the correction.\n\
- Apply spoken formatting commands instead of writing them out: \"new line\", \"new paragraph\", \
\"comma\", \"period\", \"question mark\", \"open quote\", and their equivalents in the user's \
language (\"nueva linea\", \"punto y aparte\", \"coma\", \"signo de interrogacion\"...).\n\
- If the user clearly dictates a list, format it as a list with one item per line.\n\
- Match the register to the destination: chat and messaging apps stay casual and short, email \
and documents get complete sentences and proper paragraphs, code editors and terminals get \
plain text with no smart quotes.\n\
- Never wrap the result in quotes and never add a sign-off unless it was dictated.",
    );
    if !extra.trim().is_empty() {
        p.push_str("\n\nAdditional instructions from the user:\n");
        p.push_str(extra.trim());
    }
    p
}

pub fn user_message(raw: &str, ctx: &FormatContext) -> String {
    let mut m = String::new();
    if !ctx.app_name.is_empty() {
        m.push_str(&format!("Destination app: {}\n", ctx.app_name));
    }
    if !ctx.window_title.is_empty() {
        m.push_str(&format!("Window title: {}\n", ctx.window_title));
    }
    if !ctx.language.is_empty() && ctx.language != "auto" {
        m.push_str(&format!("Language: {}\n", ctx.language));
    }
    m.push_str("\nRaw transcript:\n");
    m.push_str(raw);
    m
}

/// Strips an accidental wrapping quote pair or code fence from an LLM reply.
pub fn tidy_llm_output(s: &str) -> String {
    let mut t = s.trim();
    if t.starts_with("```") && t.ends_with("```") && t.len() > 6 {
        t = t[3..t.len() - 3].trim();
        if let Some(rest) = t.split_once('\n') {
            if !rest.0.contains(' ') {
                t = rest.1.trim();
            }
        }
    }
    let quoted = (t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\u{201C}') && t.ends_with('\u{201D}'));
    if quoted && t.chars().count() > 2 {
        let mut chars = t.chars();
        chars.next();
        chars.next_back();
        t = chars.as_str().trim();
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidies_quotes_and_fences() {
        assert_eq!(tidy_llm_output("\"hola mundo\""), "hola mundo");
        assert_eq!(tidy_llm_output("```text\nhola\n```"), "hola");
        assert_eq!(tidy_llm_output("  plain  "), "plain");
    }
}
