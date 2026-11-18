//! Embedded verified parameter (`parameters.csv`), register (`registers.csv`) and fault (`errors.csv`)
//! catalogs compiled directly into the Rust binary via `include_str!`.

use serde::{Deserialize, Serialize};

pub const PARAMETERS_CSV: &str = include_str!("../data/parameters.csv");
pub const REGISTERS_CSV: &str = include_str!("../data/registers.csv");
pub const ERRORS_CSV: &str = include_str!("../data/errors.csv");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterRecord {
    pub param_id: String,
    pub group: String,
    pub name: String,
    pub modbus_address: String,
    pub address_confidence: String,
    pub access: String,
    pub data_type: String,
    pub scale: String,
    pub unit: String,
    pub min_value: String,
    pub max_value: String,
    pub default_value: String,
    pub enum_values: String,
    pub source: String,
    pub confidence: String,
    pub verified_on_device: String,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultRecord {
    pub code: String,
    pub name: String,
    pub category: String,
    pub fault_register_value: String,
    pub description: String,
    pub action: String,
    pub source: String,
    pub confidence: String,
    pub notes: String,
}

pub fn load_embedded_parameters() -> Vec<ParameterRecord> {
    let mut rdr = csv::Reader::from_reader(PARAMETERS_CSV.as_bytes());
    rdr.deserialize().filter_map(Result::ok).collect()
}

pub fn load_embedded_faults() -> Vec<FaultRecord> {
    let mut rdr = csv::Reader::from_reader(ERRORS_CSV.as_bytes());
    rdr.deserialize().filter_map(Result::ok).collect()
}
