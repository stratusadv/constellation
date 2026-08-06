//! The integration suite for `constellation-extraction`, as one test binary.
//!
//! One module per parsed language. Each drives its extractor exactly the way
//! the indexer does, source in and an `ExtractionOutput` out, so nothing here
//! needs a store, a temporary directory, or a fixture repository.
//!
//! Two modules are not a language. [`snapshot`] pins whole extractor outputs
//! against broad fixtures, covering the behaviour no one thought to assert on,
//! and [`dump`] is the rendering it compares. A test that names one behaviour
//! belongs beside its language; a test that asks whether a refactor changed
//! anything at all belongs in [`snapshot`].

mod corpus;
mod css;
mod dump;
mod javascript;
mod python;
mod snapshot;
mod template;
