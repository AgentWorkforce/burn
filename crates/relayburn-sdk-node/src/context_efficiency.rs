//! Context-efficiency objects carried on the `summary` result. Token
//! counts cross as `BigInt` per the crate's type-mapping rules.

use napi::bindgen_prelude::BigInt;
use napi_derive::napi;

use relayburn_sdk as sdk;

use crate::u64_to_bigint;

#[napi(object)]
pub struct ContextSizeDistribution {
    pub p50: BigInt,
    pub p95: BigInt,
    pub max: BigInt,
}

#[napi(object)]
pub struct SessionContextEfficiency {
    pub session_id: String,
    pub turn_count: BigInt,
    pub context_tokens: BigInt,
    pub output_tokens: BigInt,
    pub context_tokens_per_output_token: Option<f64>,
    pub unbounded: bool,
    pub zero_output_turns_with_context: BigInt,
    pub context_size: ContextSizeDistribution,
}

#[napi(object)]
pub struct ContextEfficiencySummary {
    pub context_tokens: BigInt,
    pub output_tokens: BigInt,
    pub context_tokens_per_output_token: Option<f64>,
    pub unbounded: bool,
    pub zero_output_turns_with_context: BigInt,
    pub total_sessions: BigInt,
    pub eligible_sessions: BigInt,
    pub sessions: Vec<SessionContextEfficiency>,
}

impl From<sdk::ContextEfficiencySummary> for ContextEfficiencySummary {
    fn from(value: sdk::ContextEfficiencySummary) -> Self {
        Self {
            context_tokens: u64_to_bigint(value.context_tokens),
            output_tokens: u64_to_bigint(value.output_tokens),
            context_tokens_per_output_token: value.context_tokens_per_output_token,
            unbounded: value.unbounded,
            zero_output_turns_with_context: u64_to_bigint(value.zero_output_turns_with_context),
            total_sessions: u64_to_bigint(value.total_sessions),
            eligible_sessions: u64_to_bigint(value.eligible_sessions),
            sessions: value
                .sessions
                .into_iter()
                .map(|session| SessionContextEfficiency {
                    session_id: session.session_id,
                    turn_count: u64_to_bigint(session.turn_count),
                    context_tokens: u64_to_bigint(session.context_tokens),
                    output_tokens: u64_to_bigint(session.output_tokens),
                    context_tokens_per_output_token: session.context_tokens_per_output_token,
                    unbounded: session.unbounded,
                    zero_output_turns_with_context: u64_to_bigint(
                        session.zero_output_turns_with_context,
                    ),
                    context_size: ContextSizeDistribution {
                        p50: u64_to_bigint(session.context_size.p50),
                        p95: u64_to_bigint(session.context_size.p95),
                        max: u64_to_bigint(session.context_size.max),
                    },
                })
                .collect(),
        }
    }
}
