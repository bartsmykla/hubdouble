mod ctx;
mod params;
mod registry;
mod reply;
mod selected;

pub use ctx::{Access, Ctx, Page, PageLimit, Parts};
pub use params::{Params, Query};
pub use registry::{Entry, OperationInfo, Registry, Route};
pub use reply::{JSON, Reply};
pub use selected::selected_repositories;

use hubdouble_model::ApiError;

pub type Handler = fn(&mut Ctx<'_>) -> Result<Reply, ApiError>;

#[cfg(test)]
mod selected_tests;
#[cfg(test)]
mod tests;
