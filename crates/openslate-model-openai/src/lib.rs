pub mod client;
pub mod types;

pub mod prelude {
    pub use crate::client::OpenAICompatibleProvider;
    pub use crate::client::OpenAIProviderConfig;
    pub use crate::types::*;
}
