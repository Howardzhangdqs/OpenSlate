//! OpenSlate SQLite Store — migrations, run/step/message/prompt/trace storage.

pub mod schema;
pub mod store;

pub mod prelude {
    pub use crate::store::SqliteStore;
    pub use crate::schema;
}
