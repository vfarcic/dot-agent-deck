//! `#[spec("<area>/<sub>/<NNN>")]` — compile-time no-op annotation that
//! ties a test function to a catalog entry in PRD #77's Test Case
//! Catalog. The `xtask-linkage-check` binary text-scans for the
//! attribute; the macro itself just returns the item unchanged.

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn spec(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
