//! Offline reader and writer for OniDriveStudio `.ods` INI configuration files (UTF-8 with BOM).
//!
//! Matches `docs/ODS_FORMAT.md` in `uzbatsu-beep/A650`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const MAX_ODS_BYTES: usize = 2 * 1024 * 1024;
pub const SUPPORTED_MODELS: &[&str] = &["ПЧ ONI A650", "ПЧ ONI A650M"];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OdsError {
    #[error(".ods file exceeds 2 MB limit ({0} bytes)")]
    FileTooLarge(usize),
    #[error("file starts with ZIP/PK signature; OpenDocument spreadsheets are not OniDriveStudio INI files")]
    ZipArchiveRejected,
    #[error("missing required [General] section")]
    MissingGeneralSection,
    #[error("missing or unsupported Model in [General]: {0}")]
    UnsupportedModel(String),
    #[error("duplicate parameter key '{0}' in section [{1}]")]
    DuplicateKey(String, String),
    #[error("invalid integer value for parameter '{0}': '{1}'")]
    InvalidIntegerValue(String, String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioProfile {
    pub model: String,
    pub general: BTreeMap<String, String>,
    pub parameter_set: BTreeMap<String, i32>,
    pub parameter_read: BTreeMap<String, i32>,
    pub checked: BTreeMap<String, bool>,
}

pub fn parse_studio_profile(content: &str, expected_model: Option<&str>) -> Result<StudioProfile, OdsError> {
    if content.len() > MAX_ODS_BYTES {
        return Err(OdsError::FileTooLarge(content.len()));
    }
    if content.as_bytes().starts_with(b"PK\x03\x04") {
        return Err(OdsError::ZipArchiveRejected);
    }

    let clean = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut current_section = String::new();
    let mut has_general = false;
    let mut general = BTreeMap::new();
    let mut parameter_set = BTreeMap::new();
    let mut parameter_read = BTreeMap::new();
    let mut checked = BTreeMap::new();

    for raw_line in clean.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len() - 1].trim().to_string();
            if current_section == "General" {
                has_general = true;
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        let val = v.trim().to_string();

        match current_section.as_str() {
            "General" => {
                general.insert(key, val);
            }
            "Parameter Set" => {
                if parameter_set.contains_key(&key) {
                    return Err(OdsError::DuplicateKey(key, current_section));
                }
                let parsed: i32 = val
                    .parse()
                    .map_err(|_| OdsError::InvalidIntegerValue(key.clone(), val.clone()))?;
                parameter_set.insert(key, parsed);
            }
            "Parameter Read" => {
                if parameter_read.contains_key(&key) {
                    return Err(OdsError::DuplicateKey(key, current_section));
                }
                let parsed: i32 = val
                    .parse()
                    .map_err(|_| OdsError::InvalidIntegerValue(key.clone(), val.clone()))?;
                parameter_read.insert(key, parsed);
            }
            "Check Parameter" => {
                checked.insert(key, val == "1");
            }
            _ => {}
        }
    }

    if !has_general {
        return Err(OdsError::MissingGeneralSection);
    }

    let model = general.get("Model").cloned().unwrap_or_default();
    if !SUPPORTED_MODELS.contains(&model.as_str()) {
        return Err(OdsError::UnsupportedModel(model));
    }
    if let Some(exp) = expected_model {
        if model != exp {
            return Err(OdsError::UnsupportedModel(format!(
                "expected '{}', got '{}'",
                exp, model
            )));
        }
    }

    Ok(StudioProfile {
        model,
        general,
        parameter_set,
        parameter_read,
        checked,
    })
}

pub fn serialize_studio_profile(profile: &StudioProfile) -> String {
    let mut out = String::from("\u{feff}; Файл конфигурации ONI Drive Studio\r\n[General]\r\n");
    out.push_str(&format!("Model = {}\r\n", profile.model));
    for (k, v) in &profile.general {
        if k != "Model" {
            out.push_str(&format!("{} = {}\r\n", k, v));
        }
    }
    out.push_str("\r\n[Parameter Set]\r\n");
    for (k, v) in &profile.parameter_set {
        out.push_str(&format!("{} = {}\r\n", k, v));
    }
    out.push_str("\r\n[Parameter Read]\r\n");
    for (k, v) in &profile.parameter_read {
        out.push_str(&format!("{} = {}\r\n", k, v));
    }
    out.push_str("\r\n[Check Parameter]\r\n");
    for (k, v) in &profile.checked {
        out.push_str(&format!("{} = {}\r\n", k, if *v { 1 } else { 0 }));
    }
    out
}
