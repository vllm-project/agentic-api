pub mod conversation;
pub mod response;

pub use conversation::ConversationHandler;
pub use response::ResponseHandler;

/// Receipt created only after the existing storage transaction succeeds.
#[derive(Debug)]
pub struct CommittedResponse {
    pub response_id: String,
}
