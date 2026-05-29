extern crate alloc;

#[path = "_lex/lib.rs"]
#[allow(non_snake_case, unused_imports)]
#[allow(
    clippy::absurd_extreme_comparisons,
    clippy::collapsible_if,
    clippy::manual_strip,
    clippy::needless_update,
    clippy::new_ret_no_self,
    clippy::new_without_default,
    clippy::should_implement_trait,
    clippy::type_complexity
)]
#[rustfmt::skip]
mod _lex;

pub use _lex::*;

pub mod edges;
pub mod ids;
pub mod legacy;
pub mod record;
pub mod search;
