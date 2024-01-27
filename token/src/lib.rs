#![no_std]
#![allow(dead_code)]

mod allowance;
mod balance;
pub mod contract;
mod metadata;
mod test;

pub use crate::contract::TokenClient;
