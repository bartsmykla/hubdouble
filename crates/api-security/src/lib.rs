use hubdouble_admin::Registrar;

/// Registers what this crate owns: the Dependabot, code scanning, secret scanning, security
/// advisory, code security, code quality and dependency graph operations.
pub fn register(_: &mut Registrar<'_>) {}
