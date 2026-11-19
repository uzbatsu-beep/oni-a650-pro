//! Fail-closed SafetyGuard for ONI A650/A650M Modbus FC06 writes.
//!
//! This is an application-level interlock, NOT a functional-safety or emergency-stop
//! mechanism. A physical drive write requires an explicit operator unlock and a
//! fresh FC03 reading of 0x2100 immediately before the FC06 transaction.

use crate::catalog::ParameterRecord;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

/// Only these documented control commands are accepted.
const CONTROL_COMMANDS: &[u16] = &[1, 2, 5, 6, 7];
/// Factory reset, parameter cloning and passwords require a separate, explicit
/// procedure; they cannot be written by this generic configurator workflow.
const RESTRICTED_PARAMETERS: &[&str] = &[
    "F00.00", // User password
    "F00.04", // Initialization / factory reset
    "F00.05", // Parameter copy
    "F00.19", // Factory password
    "F00.24", // Dealer password
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SafetyError {
    #[error("Write denied by SafetyGuard: configurator is in Read-Only mode")]
    ReadOnlyMode,
    #[error("Broadcast write to slave 0 is forbidden for safety")]
    BroadcastForbidden,
    #[error("EEPROM write at 0x{0:04X} is disabled (RAM-only writes)")]
    EepromWriteForbidden(u16),
    #[error("Address 0x{0:04X} is not in the allowed writable address set")]
    AddressNotWritable(u16),
    #[error("Cannot write to 0x{0:04X}: drive state is unavailable or was not freshly verified")]
    UnknownDriveState(u16),
    #[error("Unsupported drive state {0} (0x2100); refusing FC06")]
    UnsupportedDriveState(u16),
    #[error("Parameter {code} (0x{address:04X}) may only be written with the drive STOPPED (0x2100=4), current state={state}")]
    ParameterRequiresStop { code: String, address: u16, state: u16 },
    #[error("Control command {command} is not supported for 0x2000")]
    InvalidControlCommand { command: u16 },
    #[error("Control command {command} is not permitted in drive state {state}")]
    InvalidControlState { command: u16, state: u16 },
    #[error("Frequency setpoint raw={0} exceeds the supported 0..600.00 Hz range")]
    InvalidFrequencySetpoint(u16),
    #[error("Frequency setpoint cannot be written in drive state {0}")]
    InvalidFrequencyState(u16),
    #[error("Potentially destructive or security-sensitive parameter {0} is blocked")]
    RestrictedParameter(String),
    #[error("Parameter {code}: unsupported or unresolved {field} metadata ({value})")]
    InvalidParameterMetadata { code: String, field: &'static str, value: String },
    #[error("Parameter {code}: raw={raw} is outside documented engineering range {min}..{max}")]
    ParameterOutOfRange { code: String, raw: u16, min: String, max: String },
    #[error("Parameter {code}: enum raw={raw} is not one of the documented options")]
    InvalidEnumValue { code: String, raw: u16 },
    #[error("Write outcome unknown on 0x{address:04X}: timeout waiting for FC06 echo (never auto-retried; inspect drive state first)")]
    WriteOutcomeUnknown { address: u16, value: u16 },
}

#[derive(Debug, Clone)]
pub struct SafetyGuard {
    pub read_only: bool,
    pub writable_addresses: HashSet<u16>,
}

impl Default for SafetyGuard {
    fn default() -> Self {
        // Only documented control and frequency registers, not all 0x2000..0x2007.
        let writable_addresses = [0x2000, 0x2001].into_iter().collect();
        Self { read_only: true, writable_addresses }
    }
}

impl SafetyGuard {
    /// Static checks must run BEFORE any bus traffic related to the write.
    pub fn check_address(&self, slave: u8, address: u16) -> Result<(), SafetyError> {
        if self.read_only {
            return Err(SafetyError::ReadOnlyMode);
        }
        if slave == 0 {
            return Err(SafetyError::BroadcastForbidden);
        }
        if (address & 0x8000) != 0 {
            return Err(SafetyError::EepromWriteForbidden(address));
        }
        // An empty whitelist must deny all, never become an allow-all wildcard.
        if !self.writable_addresses.contains(&address) {
            return Err(SafetyError::AddressNotWritable(address));
        }
        Ok(())
    }

    /// Authoritative write policy: caller-supplied stop-only hints are never trusted.
    ///
    /// All parameter writes require STOP (state 4), regardless of the UI mode.
    /// Physical callers must pass the state obtained from an immediate FC03 read,
    /// not a cached sample or a user-supplied value.
    pub fn check_write(
        &self,
        slave: u8,
        address: u16,
        value: u16,
        fresh_drive_state: Option<u16>,
        parameter: Option<&ParameterRecord>,
    ) -> Result<(), SafetyError> {
        self.check_address(slave, address)?;
        let state = fresh_drive_state.ok_or(SafetyError::UnknownDriveState(address))?;
        if state > 6 {
            return Err(SafetyError::UnsupportedDriveState(state));
        }

        match address {
            0x2000 => {
                if !CONTROL_COMMANDS.contains(&value) {
                    return Err(SafetyError::InvalidControlCommand { command: value });
                }
                let permitted = match value {
                    1 | 2 => state == 1 || state == 4, // Run / reverse direction
                    5 | 6 => true,                    // Stop / coast stop
                    7 => state == 4 || state == 6,     // Reset fault
                    _ => false,
                };
                if !permitted {
                    return Err(SafetyError::InvalidControlState { command: value, state });
                }
            }
            0x2001 => {
                if state != 1 && state != 4 {
                    return Err(SafetyError::InvalidFrequencyState(state));
                }
                if value > 60_000 {
                    return Err(SafetyError::InvalidFrequencySetpoint(value));
                }
            }
            _ => {
                let parameter = parameter.ok_or(SafetyError::AddressNotWritable(address))?;
                if parameter.access != "RW" {
                    return Err(SafetyError::AddressNotWritable(address));
                }
                if RESTRICTED_PARAMETERS.contains(&parameter.param_id.as_str()) {
                    return Err(SafetyError::RestrictedParameter(parameter.param_id.clone()));
                }
                if state != 4 {
                    return Err(SafetyError::ParameterRequiresStop {
                        code: parameter.param_id.clone(),
                        address,
                        state,
                    });
                }
                validate_parameter_value(parameter, value)?;
            }
        }
        Ok(())
    }
}

fn metadata_number(parameter: &ParameterRecord, field: &'static str, text: &str) -> Result<f64, SafetyError> {
    let value = text.trim().parse::<f64>().map_err(|_| SafetyError::InvalidParameterMetadata {
        code: parameter.param_id.clone(),
        field,
        value: text.to_string(),
    })?;
    if !value.is_finite() {
        return Err(SafetyError::InvalidParameterMetadata {
            code: parameter.param_id.clone(),
            field,
            value: text.to_string(),
        });
    }
    Ok(value)
}

/// Decode raw FC06 words according to the trusted embedded catalog. Dynamic
/// limits (e.g. F01.09) are deliberately rejected until read/resolve support
/// exists; silently bypassing an unresolved limit would be unsafe.
pub fn validate_parameter_value(parameter: &ParameterRecord, raw: u16) -> Result<(), SafetyError> {
    let scale = metadata_number(parameter, "scale", &parameter.scale)?;
    if scale <= 0.0 {
        return Err(SafetyError::InvalidParameterMetadata {
            code: parameter.param_id.clone(), field: "scale", value: parameter.scale.clone(),
        });
    }

    let signed = match parameter.data_type.as_str() {
        "int16" => raw as i16 as f64,
        "uint16" | "enum16" => f64::from(raw),
        _ => return Err(SafetyError::InvalidParameterMetadata {
            code: parameter.param_id.clone(),
            field: "data_type",
            value: parameter.data_type.clone(),
        }),
    };
    let engineering = signed * scale;
    let minimum = metadata_number(parameter, "min_value", &parameter.min_value)?;
    let maximum = metadata_number(parameter, "max_value", &parameter.max_value)?;
    if minimum > maximum {
        return Err(SafetyError::InvalidParameterMetadata {
            code: parameter.param_id.clone(), field: "range", value: format!("{}..{}", minimum, maximum),
        });
    }
    if engineering + 1e-9 < minimum || engineering - 1e-9 > maximum {
        return Err(SafetyError::ParameterOutOfRange {
            code: parameter.param_id.clone(),
            raw,
            min: parameter.min_value.clone(),
            max: parameter.max_value.clone(),
        });
    }

    if parameter.data_type == "enum16" {
        let choices: HashSet<u16> = parameter.enum_values
            .split(';')
            .filter_map(|text| text.split_once('='))
            .filter_map(|(code, _)| code.trim().parse::<u16>().ok())
            .collect();
        // Missing or partial enumerations are not treated as a wildcard.
        if !choices.contains(&raw) {
            return Err(SafetyError::InvalidEnumValue { code: parameter.param_id.clone(), raw });
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parameter(code: &str, kind: &str, minimum: &str, maximum: &str, enums: &str) -> ParameterRecord {
        ParameterRecord {
            param_id: code.to_string(), group: "F01".to_string(), name: "Test".to_string(),
            modbus_address: "0x0101".to_string(), address_confidence: "high".to_string(),
            access: "RW".to_string(), data_type: kind.to_string(), scale: "0.01".to_string(),
            unit: "Hz".to_string(), min_value: minimum.to_string(), max_value: maximum.to_string(),
            default_value: "0".to_string(), enum_values: enums.to_string(),
            source: "test".to_string(), confidence: "high".to_string(),
            verified_on_device: "no".to_string(), notes: String::new(),
        }
    }

    #[test]
    fn read_only_broadcast_eeprom_and_unlisted_addresses_are_denied() {
        let mut guard = SafetyGuard::default();
        assert_eq!(guard.check_address(1, 0x2000), Err(SafetyError::ReadOnlyMode));
        guard.read_only = false;
        assert_eq!(guard.check_address(0, 0x2000), Err(SafetyError::BroadcastForbidden));
        assert_eq!(guard.check_address(1, 0x8101), Err(SafetyError::EepromWriteForbidden(0x8101)));
        assert_eq!(guard.check_address(1, 0x2002), Err(SafetyError::AddressNotWritable(0x2002)));
        guard.writable_addresses.clear();
        assert_eq!(guard.check_address(1, 0x2000), Err(SafetyError::AddressNotWritable(0x2000)));
    }

    #[test]
    fn write_requires_known_fresh_state() {
        let mut guard = SafetyGuard::default();
        guard.read_only = false;
        assert_eq!(guard.check_write(1, 0x2000, 1, None, None), Err(SafetyError::UnknownDriveState(0x2000)));
        assert_eq!(guard.check_write(1, 0x2000, 1, Some(65535), None), Err(SafetyError::UnsupportedDriveState(65535)));
    }

    #[test]
    fn control_values_and_state_transitions_are_restricted() {
        let mut guard = SafetyGuard::default();
        guard.read_only = false;
        assert!(guard.check_write(1, 0x2000, 1, Some(4), None).is_ok());
        assert!(guard.check_write(1, 0x2000, 2, Some(1), None).is_ok());
        assert!(guard.check_write(1, 0x2000, 5, Some(2), None).is_ok());
        assert!(guard.check_write(1, 0x2000, 6, Some(3), None).is_ok());
        assert!(guard.check_write(1, 0x2000, 7, Some(6), None).is_ok());
        assert!(matches!(guard.check_write(1, 0x2000, 3, Some(4), None), Err(SafetyError::InvalidControlCommand { .. })));
        assert!(matches!(guard.check_write(1, 0x2000, 1, Some(6), None), Err(SafetyError::InvalidControlState { .. })));
        assert!(matches!(guard.check_write(1, 0x2000, 7, Some(1), None), Err(SafetyError::InvalidControlState { .. })));
    }

    #[test]
    fn frequency_setpoint_has_raw_bounds() {
        let mut guard = SafetyGuard::default();
        guard.read_only = false;
        assert!(guard.check_write(1, 0x2001, 5000, Some(1), None).is_ok());
        assert_eq!(guard.check_write(1, 0x2001, 60_001, Some(4), None),
            Err(SafetyError::InvalidFrequencySetpoint(60_001)));
        assert_eq!(guard.check_write(1, 0x2001, 5000, Some(3), None),
            Err(SafetyError::InvalidFrequencyState(3)));
    }

    #[test]
    fn all_parameter_writes_require_stop_and_trusted_catalog() {
        let mut guard = SafetyGuard::default();
        guard.read_only = false;
        guard.writable_addresses.insert(0x0101);
        let p = parameter("F01.01", "uint16", "0.00", "60.00", "");
        assert!(matches!(guard.check_write(1, 0x0101, 5000, Some(1), Some(&p)),
            Err(SafetyError::ParameterRequiresStop { .. })));
        assert!(guard.check_write(1, 0x0101, 5000, Some(4), Some(&p)).is_ok());
        assert!(matches!(guard.check_write(1, 0x0101, 6500, Some(4), Some(&p)),
            Err(SafetyError::ParameterOutOfRange { .. })));
        assert_eq!(guard.check_write(1, 0x0101, 5000, Some(4), None),
            Err(SafetyError::AddressNotWritable(0x0101)));
    }

    #[test]
    fn signed_limits_and_enums_are_validated() {
        let mut p = parameter("F01.02", "int16", "-50.00", "50.00", "");
        assert!(validate_parameter_value(&p, (-5000_i16) as u16).is_ok());
        assert!(matches!(validate_parameter_value(&p, (-6000_i16) as u16),
            Err(SafetyError::ParameterOutOfRange { .. })));
        p.data_type = "enum16".to_string();
        p.scale = "1".to_string();
        p.min_value = "0".to_string();
        p.max_value = "3".to_string();
        p.enum_values = "0=none;1=first;3=last".to_string();
        assert!(validate_parameter_value(&p, 3).is_ok());
        assert!(matches!(validate_parameter_value(&p, 2), Err(SafetyError::InvalidEnumValue { .. })));
    }

    #[test]
    fn dynamic_catalog_bounds_fail_closed() {
        let p = parameter("F01.02", "uint16", "0", "F01.08", "");
        assert!(matches!(validate_parameter_value(&p, 4000),
            Err(SafetyError::InvalidParameterMetadata { field: "max_value", .. })));
    }

    #[test]
    fn restricted_parameters_are_not_writable() {
        let mut guard = SafetyGuard::default();
        guard.read_only = false;
        guard.writable_addresses.insert(0x0004);
        let p = parameter("F00.04", "enum16", "0", "6", "0=None;1=Reset");
        assert_eq!(guard.check_write(1, 0x0004, 0, Some(4), Some(&p)),
            Err(SafetyError::RestrictedParameter("F00.04".into())));
    }
}
