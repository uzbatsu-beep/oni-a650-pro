//! Industrial SafetyGuard & Audit Log for ONI A650 / A650M.
//!
//! Enforces:
//! 1. Default `read_only = true` mode.
//! 2. Prohibition of broadcast writes (`slave == 0`).
//! 3. Run-time lock check (`× Stop-only` parameters cannot be written while `drive_state == 1`).
//! 4. `WriteOutcomeUnknown` error when an `FC06` write times out (never auto-retried!).

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SafetyError {
    #[error("Write denied by SafetyGuard: configurator is in Read-Only mode")]
    ReadOnlyMode,
    #[error("Broadcast write to slave 0 is forbidden for safety")]
    BroadcastForbidden,
    #[error("Address 0x{0:04X} is not in the allowed writable address set")]
    AddressNotWritable(u16),
    #[error("Parameter {code} (0x{address:04X}) has Stop-Only (×) attribute and cannot be written while the motor is running")]
    StopOnlyWhileRunning { code: String, address: u16 },
    #[error("Write outcome unknown on 0x{address:04X}: timeout waiting for FC06 echo (never auto-retried; inspect drive state first)")]
    WriteOutcomeUnknown { address: u16, value: u16 },
}

#[derive(Debug, Clone)]
pub struct SafetyGuard {
    pub read_only: bool,
    pub allow_broadcast_writes: bool,
    pub writable_addresses: HashSet<u16>,
}

impl Default for SafetyGuard {
    fn default() -> Self {
        let mut writable = HashSet::new();
        // Control registers 0x2000..0x2007 are writable when unlocked
        for addr in 0x2000..=0x2007 {
            writable.insert(addr);
        }
        Self {
            read_only: true,
            allow_broadcast_writes: false,
            writable_addresses: writable,
        }
    }
}

impl SafetyGuard {
    pub fn check_write(
        &self,
        slave: u8,
        address: u16,
        is_running: bool,
        stop_only_param: Option<&str>,
    ) -> Result<(), SafetyError> {
        if self.read_only {
            return Err(SafetyError::ReadOnlyMode);
        }
        if slave == 0 && !self.allow_broadcast_writes {
            return Err(SafetyError::BroadcastForbidden);
        }
        if is_running {
            if let Some(code) = stop_only_param {
                return Err(SafetyError::StopOnlyWhileRunning {
                    code: code.to_string(),
                    address,
                });
            }
        }
        if !self.writable_addresses.is_empty() && !self.writable_addresses.contains(&address) {
            return Err(SafetyError::AddressNotWritable(address));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    pub ts: String,
    pub op: String,
    pub slave: u8,
    pub model: String,
    pub details: serde_json::Value,
}

impl AuditLogEntry {
    pub fn new(op: &str, slave: u8, model: &str, details: serde_json::Value) -> Self {
        Self {
            ts: Utc::now().to_rfc3339(),
            op: op.to_string(),
            slave,
            model: model.to_string(),
            details,
        }
    }
}
