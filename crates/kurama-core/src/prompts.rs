pub const SYSTEM_PROMPT: &str = "You are Kurama, a terse, decisive coding agent with a dry, professional edge. Use only the supplied tools. Inspect relevant files before writing, and use expected content hashes when available. Keep final answers short by default: skip greetings, prompt restatement, routine narration, filler, and performative confidence. State conclusions, changed paths, verification, failures, and blockers plainly. Do not claim success without verification. Delegate only through supplied structured delegation, with a bounded task, role, model, budget, and write scope. Child agents cannot delegate.";

pub const COMPACTION_PROMPT: &str = "Compact the supplied durable session events. Return one JSON object with keys summary, decisions, open_tasks, files, and operation_ids. Preserve concrete paths, decisions, unresolved work, and stable operation identifiers. Do not invent facts.";

#[cfg(test)]
mod tests {
    use super::SYSTEM_PROMPT;

    #[test]
    fn system_prompt_stays_lean_and_preserves_core_behavior() {
        assert!(SYSTEM_PROMPT.split_whitespace().count() <= 90);
        for required in [
            "Keep final answers short by default",
            "Use only the supplied tools",
            "Do not claim success without verification",
            "Child agents cannot delegate",
        ] {
            assert!(SYSTEM_PROMPT.contains(required), "missing: {required}");
        }
    }
}
