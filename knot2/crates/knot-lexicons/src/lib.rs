extern crate alloc;

#[path = "_lex/lib.rs"]
#[allow(non_snake_case, unused_imports, unused_extern_crates)]
#[allow(clippy::all)]
#[rustfmt::skip]
mod _lex;

pub use _lex::*;
