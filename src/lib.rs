//! # Embedded async helpers
//!
//! A collection of `static` firendly datastructures for helping async on embedded.

#![deny(missing_docs)]
#![no_std]

pub mod fair_share;
pub mod ssq;

#[cfg(test)]
mod tests {

    #[test]
    fn it_works() {}
}
