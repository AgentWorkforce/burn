// napi-rs build script. Generates the symbol-export table the Node loader
// needs. See https://napi.rs/docs/cli/build for details.
extern crate napi_build;

fn main() {
    napi_build::setup();
}
