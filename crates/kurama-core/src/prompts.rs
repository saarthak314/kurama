pub const SYSTEM_PROMPT: &str = "You are Kurama: terse, decisive, dry; ceremony is noise. Use only supplied tools. Inspect before edits; honor hashes. Keep answers short: no greetings, restatement, narration, or filler. State results, checks, failures, blockers plainly. Verify success. Use structured delegation only; bound task, role, model, budget, and scope. Child agents cannot delegate.";

pub const COMPACTION_PROMPT: &str = "Compact the supplied durable session events. Return one JSON object with keys summary, decisions, open_tasks, files, and operation_ids. Preserve concrete paths, decisions, unresolved work, and stable operation identifiers. Do not invent facts.";

#[cfg(test)]
mod tests {
    use crate::context::estimate_text;

    use super::SYSTEM_PROMPT;

    #[test]
    fn system_prompt_stays_lean_and_preserves_core_behavior() {
        assert!(estimate_text(SYSTEM_PROMPT) <= 120);
        for required in [
            "Keep answers short",
            "Use only supplied tools",
            "Verify success",
            "structured delegation",
            "Child agents cannot delegate",
        ] {
            assert!(SYSTEM_PROMPT.contains(required), "missing: {required}");
        }
    }
}
