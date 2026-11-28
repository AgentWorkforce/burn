#!/usr/bin/env bash
set -euo pipefail

# rust-oleum needs fresh workspace diagnostics even on a warm CI cache.
cargo clean -p relayburn-cli -p relayburn-sdk -p relayburn-sdk-node -p relayburn-test-seal >&2
# Enable the seven default lint groups in the pinned rust-oleum 0.4 preset.
cargo clippy --locked --workspace --all-targets --message-format=json -- \
  -W clippy::unwrap_used -W clippy::panic_in_result_fn \
  -W missing_docs -W clippy::missing_errors_doc -W clippy::missing_panics_doc \
  -W clippy::fn_params_excessive_bools -W clippy::struct_excessive_bools \
  -W clippy::ptr_arg -W clippy::clone_on_ref_ptr -W missing_debug_implementations -W unreachable_pub \
  -W clippy::as_conversions -W clippy::cast_possible_truncation -W clippy::cast_sign_loss \
  -W clippy::undocumented_unsafe_blocks -W clippy::multiple_unsafe_ops_per_block -W unsafe_op_in_unsafe_fn \
  -W clippy::allow_attributes -W clippy::allow_attributes_without_reason \
  -W clippy::dbg_macro -W clippy::todo -W clippy::print_stdout -W clippy::print_stderr
