//! OpenSlate SQLite Store — migrations, run/step/message/prompt/trace storage.

pub mod codec;
pub mod query;
pub mod recorder;
pub mod schema;
pub mod store;
pub mod write;

pub mod prelude {
    pub use crate::codec;
    pub use crate::query::*;
    pub use crate::recorder::{RunFinal, RunRecorder};
    pub use crate::schema;
    pub use crate::store::SqliteStore;
    pub use crate::write;
}
