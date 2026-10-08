//! opencode-specific refinements of the shared evidence mapping.

use super::Context;
use crate::source::SessionRecords;

/// Derive what the shared mapping cannot for opencode sessions.
pub(super) fn refine(_ctx: &Context<'_>, _records: &mut SessionRecords) {}
