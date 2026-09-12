// Platform-independent state manager: ProfileStore, reducer, persistence.
// Must compile without the `windows` crate, so it can be built and tested on
// any OS.

pub mod json_repository;
pub mod manager;
pub mod repository;
pub mod store;

pub use json_repository::JsonFileRepository;
pub use manager::{Dispatch, PersistRequest, PersistResult, StateManager, PERSIST_DEBOUNCE};
pub use repository::{InMemoryRepository, ProfileRepository, RepoError};
pub use store::{EndpointProfile, ProfileStore, SessionEntry, Settings};
