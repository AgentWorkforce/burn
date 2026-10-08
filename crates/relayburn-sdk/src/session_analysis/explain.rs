//! Why each finding costs tokens, and the change that avoids it.

/// `(why it costs tokens, suggested change)` for a finding code.
pub(super) fn guidance(code: &str) -> (&'static str, &'static str) {
    match code {
        "retry-loop" => (
            "Every retry resends the whole conversation as input, so repeating a failing call multiplies context spend without new information.",
            "Stop after the first identical failure: read the error and change the arguments or approach. If the agent keeps retrying this command, document its failure mode in the project instructions.",
        ),
        "failure-run" => (
            "Each failed call still pays for the full context plus the error output it appends to it.",
            "Fix the first error of the run (missing dependency, wrong directory, permissions) before letting the agent continue, or have it ask for help after two failures.",
        ),
        "cancellation-run" => (
            "Cancelled calls were paid for but produced nothing the session kept.",
            "Find why the calls were interrupted (timeouts, user interrupts) and give long-running commands a narrower scope or an explicit timeout.",
        ),
        "compaction-loss" => (
            "Compaction discards the cached prefix: later turns rebuild cache at write rates, and work summarized away is no longer visible to the model.",
            "Split long work into subagents or fresh sessions before the context fills, and keep durable facts in project instruction files so they survive compaction.",
        ),
        "edit-revert" => (
            "The tokens spent writing, applying and then undoing the edit bought nothing.",
            "Have the agent read the surrounding code and confirm the approach before editing; plan multi-step changes up front.",
        ),
        "edit-heavy" => (
            "Editing without reading leads to failed or reverted edits, and each retry pays for the full context again.",
            "Instruct the agent to read a file's surrounding context before changing it.",
        ),
        "skill-recall-dup" => (
            "Each call re-injects the whole skill body into context, where it rides along for the rest of the session.",
            "Invoke the skill once per session and refer back to it; drop the repeated invocations from prompts or workflows.",
        ),
        "skill-pruning-protection" => (
            "Protected skill output is never pruned, so it is re-read from cache on every later turn.",
            "Trim the skill body to what the task needs, or invoke it inside a subagent so its body leaves the main context.",
        ),
        "system-prompt-tax" => (
            "The system prompt and skill catalog are re-read from cache on every turn of the session.",
            "Disable skills, agents and MCP servers the project does not use to shrink the catalog.",
        ),
        "tool-output-bloat" => (
            "Large tool output is appended to context and re-read from cache on every later turn.",
            "Cap the output (head/tail, quiet flags, an output-length limit) or write verbose output to a file and search it.",
        ),
        code if code.starts_with("ghost-") => (
            "Installed agents, commands and skills are described in the system prompt of every session, used or not.",
            "Archive or delete the unused file (the archive action moves it out of the always-loaded surface) and reinstall it only in the projects that use it.",
        ),
        "tool-call-pattern" => (
            "A run of single-purpose tool calls re-sends the context once per step.",
            "Batch the steps into one call (one broader search, one multi-file edit, one combined git status/diff) or a project script.",
        ),
        "unpriced-usage" => (
            "burn has no price for this model, so its spend is reported as unknown, never as free.",
            "Add the model to a models.dev-format pricing file and pass it with --pricing.",
        ),
        "instruction-overhead" => (
            "Project instruction files are loaded at session start and re-read from cache on every turn.",
            "Shorten or remove the section, or move it into a document the agent loads only when the task needs it.",
        ),
        "context-growth" => (
            "Everything added to the context window is re-read on every later inference.",
            "Narrow what the call returns (read file ranges, filter or truncate command output) or run it in a subagent so only its summary returns.",
        ),
        "max-tokens-stop" => (
            "A turn cut off at the output limit is usually continued or retried, paying for the context again.",
            "Ask for smaller increments of output, or raise the model's output limit for this work.",
        ),
        "refusal" => (
            "A refused turn spends tokens without progress.",
            "Rephrase the request or supply the context the model needs to proceed.",
        ),
        "usage-unrecorded" => (
            "burn cannot count or price tokens the transcript did not record, so these turns are missing from every total.",
            "Analyze the harness's complete transcript; if the harness writes usage elsewhere, its capture is a relayhistory gap to report.",
        ),
        "attribution-unavailable" => (
            "Without per-tool evidence burn cannot say which files, commands or subagents drove the spend.",
            "Analyze a transcript that records tool calls and tool results.",
        ),
        _ => (
            "",
            "Inspect the turns in the evidence and remove the repeated work the title names.",
        ),
    }
}
