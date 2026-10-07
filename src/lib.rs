//! convolith — loss-preserving, provenance-first canonicalizer for fragmented AI
//! conversation history.

pub mod archive;
pub mod artifacts;
pub mod cli;
pub mod collect;
pub mod config;
pub mod dataset;
pub mod dedup;
pub mod discover;
pub mod id;
pub mod identity;
pub mod importer;
pub mod ledger;
pub mod leveldb;
pub mod model;
pub mod parser;
pub mod parsers;
pub mod report;
pub mod scratch;
pub mod search;
pub mod secrets;
pub mod source;
pub mod sqlite;
pub mod timeutil;
pub mod validate;
