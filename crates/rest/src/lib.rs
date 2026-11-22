mod ctx;
mod params;
mod registry;
mod reply;

pub use ctx::{Access, Ctx, Page, Parts};
pub use params::{Params, Query};
pub use registry::{Entry, OperationInfo, Registry, Route};
pub use reply::{JSON, Reply};

use hubdouble_model::ApiError;

pub type Handler = fn(&mut Ctx<'_>) -> Result<Reply, ApiError>;

#[cfg(test)]
mod tests;
