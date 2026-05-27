pub mod client;
pub mod stream;
pub mod types;

pub mod prelude {
    pub use crate::client::OpenAICompatibleProvider;
    pub use crate::client::OpenAIProviderConfig;
    pub use crate::stream::ModelStreamEvent;
    pub use crate::types::*;
}
