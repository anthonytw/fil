//! The agent relay (DESIGN.md section 10, `docs/m2/plan.md` A1): the message [`filter`] the
//! client applies (the trusted end) and the [`relay`] between an `Agent` channel and a local
//! socket. The session's socket (`fil-session`) and the client's wiring (`fil-client`) use these.

pub mod filter;
pub mod relay;

pub use filter::KeyInfo;
pub use relay::{
    AgentUse, Bind, BindKey, CONFIRM_TIMEOUT, Confirm, CountingStream, KeyUse, KeyUses,
    RequestCounts, Rules, connect_local, relay,
};
