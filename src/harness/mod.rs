//! The verification harness: a seeded corpus ([`corpus`]), datasets kept in GCS ([`dataset`]) and the
//! portable-core checks run against them ([`checks`]).

pub mod checks;
pub mod corpus;
pub mod dataset;
