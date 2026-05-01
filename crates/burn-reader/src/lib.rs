//! `burn-reader` — pure parsers for harness session logs.
//!
//! Mirrors `packages/reader/src/` from the TypeScript workspace. Planned
//! modules (filed as sub-issues under #222):
//!
//! - `types`         — `TurnRecord`, `ContentRecord`, `ActivityCategory`, `Harness`
//! - `claude`        — Claude Code session log parser (was `claude.ts`)
//! - `codex`         — Codex session log parser (was `codex.ts`)
//! - `opencode`      — OpenCode session log parser (was `opencode.ts`)
//! - `opencode_stream` — OpenCode incremental stream parser (was `opencode-stream.ts`)
//! - `classifier`    — activity classification rule tables (was `classifier.ts`)
//! - `git`           — git project canonicalization (was `git.ts`)
//! - `hash`          — content fingerprinting (was `hash.ts`)
//! - `fidelity`      — fidelity scoring (was `fidelity.ts`)
//! - `user_turn`     — user-turn extraction (was `userTurn.ts`)
//!
//! Conformance gate: every Rust module must produce byte-identical output
//! to its TypeScript counterpart on the existing `*.test.ts` fixture corpus.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_compiles() {}
}
