//! ONI A650 / A650M Modbus RTU register address arithmetic (`F00..F20`, `H00`, `U00..U01`).
//!
//! F and U addresses follow ONI A650M Manual, Edition 3, Appendix A.4.3.
//! H00 RAM/EEPROM pairs are recorded in the A650M OniDriveStudio table.
//! This module also provides normalization for legacy `OniDriveStudio v1.2.7.0` EXE catalog defects:
//! - Duplicate code `F17.28` at `0x111D` -> `F17.29`;
//! - Motor 2 group `F18.08..F18.30` off-by-two (`-2`) address shift in legacy `.ods` profiles.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("unsupported parameter code format: {0}")]
    InvalidFormat(String),
    #[error("undocumented parameter group in code: {0}")]
    UndocumentedGroup(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterAddresses {
    pub ram: u16,
    pub eeprom: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedStudioEntry {
    pub canonical_code: String,
    pub addresses: ParameterAddresses,
    pub audit_note: Option<String>,
}

/// Computes `(ram_address, eeprom_address)` for an ONI A650 / A650M parameter code.
///
/// - `F00.00`..`F31.99`: RAM `(group << 8) | index`, EEPROM `((group + 0x80) << 8) | index`
/// - `H00.00`..`H00.99`: RAM `0x4000 | index`, EEPROM `0xC000 | index`
/// - `U00.00`..`U01.99`: RAM `((0x30 + group) << 8) | index`, EEPROM `None` (read-only)
pub fn parameter_addresses(code: &str) -> Result<ParameterAddresses, AddressError> {
    let trimmed = code.trim();
    if trimmed.len() != 6 || &trimmed[3..4] != "." {
        return Err(AddressError::InvalidFormat(code.to_string()));
    }

    let family = trimmed.chars().next().unwrap();
    let group: u16 = trimmed[1..3]
        .parse()
        .map_err(|_| AddressError::InvalidFormat(code.to_string()))?;
    let index: u16 = trimmed[4..6]
        .parse()
        .map_err(|_| AddressError::InvalidFormat(code.to_string()))?;

    match (family, group) {
        ('F', 0..=31) => {
            let ram = (group << 8) | index;
            Ok(ParameterAddresses {
                ram,
                eeprom: Some(ram | 0x8000),
            })
        }
        ('H', 0) => {
            let ram = 0x4000 | index;
            Ok(ParameterAddresses {
                ram,
                eeprom: Some(0xC000 | index),
            })
        }
        ('U', 0..=1) => {
            let ram = ((0x30 + group) << 8) | index;
            Ok(ParameterAddresses { ram, eeprom: None })
        }
        _ => Err(AddressError::UndocumentedGroup(code.to_string())),
    }
}

/// Normalizes a raw catalog entry extracted from `OniDriveStudio v1.2.3.0.exe` (build 1.2.7.0).
pub fn normalize_studio_catalog_entry(
    raw_code: &str,
    legacy_ram: u16,
    name: &str,
) -> Result<NormalizedStudioEntry, AddressError> {
    let mut canonical_code = raw_code.trim().to_string();
    let mut audit_note = None;

    if canonical_code == "F17.28" && (legacy_ram == 0x111D || name.contains("29")) {
        canonical_code = "F17.29".to_string();
        audit_note = Some(
            "Fixed duplicate code F17.28 -> F17.29 at 0x111D (EXE v1.2.7.0 bug)".to_string(),
        );
    }

    let addresses = parameter_addresses(&canonical_code)?;
    if legacy_ram != addresses.ram {
        audit_note = Some(format!(
            "Corrected legacy EXE RAM address 0x{:04X} -> 0x{:04X} (Appendix A.4.3 formula)",
            legacy_ram, addresses.ram
        ));
    }

    Ok(NormalizedStudioEntry {
        canonical_code,
        addresses,
        audit_note,
    })
}
