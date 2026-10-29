// Cargo does not run unit tests embedded in build scripts. Compile them as a
// normal integration-test target; the other build-script functions are unused here.
#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;
