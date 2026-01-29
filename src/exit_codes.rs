// ABOUTME: Exit code constants matching the jjq specification.
// ABOUTME: Used throughout the codebase for consistent process exit codes.

use std::fmt;

pub const CONFLICT: i32 = 1;
pub const CHECK_FAILED: i32 = 2;
pub const LOCK_HELD: i32 = 3;
pub const TRUNK_MOVED: i32 = 4;
pub const USAGE: i32 = 10;

/// Error type that carries a specific exit code.
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub message: String,
}

impl ExitError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        ExitError { code, message: message.into() }
    }
}

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExitError {}
