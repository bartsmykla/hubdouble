use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "--baseline") {
        eprintln!(
            "baseline schema_bytes={}",
            hubdouble_spec::GRAPHQL_SCHEMA.len()
        );
        return ExitCode::SUCCESS;
    }
    let start = Instant::now();
    let schema = match hubdouble_gql::Schema::load() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("schema load failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let elapsed = start.elapsed();
    eprintln!(
        "types={} load_ms={:.2}",
        schema.type_count(),
        elapsed.as_secs_f64() * 1000.0
    );
    ExitCode::SUCCESS
}
