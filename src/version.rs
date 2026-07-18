/// Version string baked in at compile time.
///
/// Local builds (`cargo build`, `cargo install`) show "dev"; the release
/// workflow sets GITBATCH_VERSION to the git tag of the GitHub release.
pub const VERSION: &str = match option_env!("GITBATCH_VERSION") {
    Some(v) => v,
    None => "dev",
};
