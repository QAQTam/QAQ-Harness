//! services/ — shared service modules consumed by the Ring engines.
//!
//! Each module provides a focused capability. None of these modules contain
//! event-loop logic; they are called synchronously from within engine handlers.
//!
//! ## Modules
//!
//! | Module           | Role                            |
//! |------------------|---------------------------------|
//! | `conflict.rs`    | Write-conflict detection        |
//! | `dashboard.rs`   | Status / metrics reporting      |

pub(crate) mod conflict;
pub(crate) mod dashboard;
