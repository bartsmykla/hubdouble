use std::process::Command;

#[test]
fn the_binary_runs_mimalloc_without_eager_arena_commit() {
    // Given the built binary, with no environment override of the option
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hubdouble"));
    cmd.arg("help")
        .env("MIMALLOC_VERBOSE", "1")
        .env_remove("MIMALLOC_ARENA_EAGER_COMMIT")
        .env_remove("MIMALLOC_EAGER_REGION_COMMIT");

    // When it starts and mimalloc prints its options
    let out = cmd.output().unwrap_or_else(|e| panic!("{e}"));
    let options = String::from_utf8_lossy(&out.stderr);

    // Then arena_eager_commit is 0, as the define in .cargo/config.toml sets it
    let value = options
        .lines()
        .find_map(|l| l.strip_prefix("option 'arena_eager_commit': "))
        .map(str::trim);
    assert_eq!(value, Some("0"));
}
