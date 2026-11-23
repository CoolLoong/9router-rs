#[cfg(feature = "node-bindings")]
#[path = "lib.rs"]
mod generated;

#[cfg(feature = "node-bindings")]
pub use generated::*;

#[cfg(not(feature = "node-bindings"))]
pub mod js;
#[cfg(not(feature = "node-bindings"))]
pub mod port;
