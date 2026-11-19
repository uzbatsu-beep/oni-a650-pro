//! `a650_core` — Rust library and Tauri v2 backend for ONI A650 / A650M Configurator PRO 2.0.

pub mod actor;
pub mod addresses;
pub mod catalog;
pub mod modbus;
pub mod ods;
pub mod safety;

#[cfg(test)]
mod tests {
    use super::actor::decode_output_frequency_hz;
    use super::addresses::{normalize_studio_catalog_entry, parameter_addresses};
    use super::catalog::{load_embedded_faults, load_embedded_parameters};
    use super::modbus::{
        crc16_modbus, parse_read_response, parse_write_response, ReadRegistersRequest,
        WriteRegisterRequest,
    };
    use super::ods::{parse_studio_profile, serialize_studio_profile};
    use super::safety::{SafetyError, SafetyGuard};

    #[test]
    fn test_signed_frequency_u00_00_f15_08_default_mode() {
        assert_eq!(decode_output_frequency_hz(0x1388), 50.0);
        assert_eq!(decode_output_frequency_hz(0xEC78), -50.0);
        assert_eq!(decode_output_frequency_hz(0x0000), 0.0);
        assert_eq!(decode_output_frequency_hz(0xFFFF), -0.01);
        assert_eq!(decode_output_frequency_hz(0x8000), -327.68);
    }

    #[test]
    fn test_address_arithmetic_f_h_u_groups() {
        // Standard F-group: F13.01 -> RAM 0x0D01, EEPROM 0x8D01
        let f13_01 = parameter_addresses("F13.01").unwrap();
        assert_eq!(f13_01.ram, 0x0D01);
        assert_eq!(f13_01.eeprom, Some(0x8D01));

        // Pump cascade H00 group: H00.13 -> RAM 0x400D, EEPROM 0xC00D
        let h00_13 = parameter_addresses("H00.13").unwrap();
        assert_eq!(h00_13.ram, 0x400D);
        assert_eq!(h00_13.eeprom, Some(0xC00D));

        // Monitoring U00 group: U00.00 -> RAM 0x3000, EEPROM None
        let u00_00 = parameter_addresses("U00.00").unwrap();
        assert_eq!(u00_00.ram, 0x3000);
        assert_eq!(u00_00.eeprom, None);
    }

    #[test]
    fn test_normalize_legacy_exe_bugs_f17_and_f18() {
        // Duplicate F17.28 at 0x111D -> F17.29
        let fixed_f17 = normalize_studio_catalog_entry(
            "F17.28",
            0x111D,
            "Код функции, определяемой пользователем 29",
        )
        .unwrap();
        assert_eq!(fixed_f17.canonical_code, "F17.29");
        assert_eq!(fixed_f17.addresses.ram, 0x111D);
        assert!(fixed_f17.audit_note.is_some());

        // Motor 2 F18.30 shifted to 0x121C in EXE -> corrected to 0x121E
        let fixed_f18 =
            normalize_studio_catalog_entry("F18.30", 0x121C, "Автонастройка двигателя 2")
                .unwrap();
        assert_eq!(fixed_f18.canonical_code, "F18.30");
        assert_eq!(fixed_f18.addresses.ram, 0x121E);
        assert_eq!(fixed_f18.addresses.eeprom, Some(0x921E));
    }

    #[test]
    fn test_golden_modbus_rtu_frames() {
        // Golden frame 1: Set frequency 30.00 Hz (0x0BB8) to 0x2001 on slave 1
        let req_freq = WriteRegisterRequest::new(1, 0x2001, 3000).unwrap();
        let bytes = req_freq.to_bytes();
        assert_eq!(bytes, [0x01, 0x06, 0x20, 0x01, 0x0B, 0xB8, 0xD4, 0x88]);
        assert!(parse_write_response(req_freq, &bytes).is_ok());

        // Golden frame 2: Read U00.00 (0x3000, count 1) on slave 1
        let req_read = ReadRegistersRequest::new(1, 0x3000, 1).unwrap();
        assert_eq!(
            req_read.to_bytes(),
            [0x01, 0x03, 0x30, 0x00, 0x00, 0x01, 0x8B, 0x0A]
        );

        // Simulated response for 50.00 Hz (5000 = 0x1388)
        let mut resp = vec![0x01, 0x03, 0x02, 0x13, 0x88, 0x00, 0x00];
        let crc = crc16_modbus(&resp[..5]);
        resp[5] = (crc & 0xFF) as u8;
        resp[6] = (crc >> 8) as u8;
        let vals = parse_read_response(1, 1, &resp).unwrap();
        assert_eq!(vals, vec![5000]);
    }

    #[test]
    fn test_safety_guard_rules() {
        let mut guard = SafetyGuard::default();
        // Blocked by default read_only = true
        assert_eq!(
            guard.check_write(1, 0x2000, false, None),
            Err(SafetyError::ReadOnlyMode)
        );

        guard.read_only = false;
        // Broadcast slave 0 blocked
        assert_eq!(
            guard.check_write(0, 0x2000, false, None),
            Err(SafetyError::BroadcastForbidden)
        );

        // Stop-only parameter blocked while motor is running
        assert_eq!(
            guard.check_write(1, 0x0101, true, Some("F01.01")),
            Err(SafetyError::StopOnlyWhileRunning {
                code: "F01.01".to_string(),
                address: 0x0101
            })
        );
    }

    #[test]
    fn test_ods_profile_roundtrip() {
        let sample = "\u{feff}; Файл конфигурации ONI Drive Studio\r\n\
                      [General]\r\n\
                      Model = ПЧ ONI A650M\r\n\
                      Address = 1\r\n\
                      [Parameter Set]\r\n\
                      F01.01 = 6\r\n\
                      H00.13 = 240\r\n\
                      [Parameter Read]\r\n\
                      [Check Parameter]\r\n\
                      F01.01 = 1\r\n";
        let parsed = parse_studio_profile(sample, Some("ПЧ ONI A650M")).unwrap();
        assert_eq!(parsed.parameter_set.get("F01.01"), Some(&6));
        assert_eq!(parsed.parameter_set.get("H00.13"), Some(&240));
        assert_eq!(parsed.checked.get("F01.01"), Some(&true));

        let dumped = serialize_studio_profile(&parsed);
        let reparsed = parse_studio_profile(&dumped, None).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn test_embedded_catalogs_loaded() {
        let params = load_embedded_parameters();
        let faults = load_embedded_faults();
        assert_eq!(params.len(), 617);
        assert_eq!(faults.len(), 45);
        assert!(params.iter().any(|p| p.param_id == "F17.29"));
        assert!(params.iter().any(|p| p.param_id == "H00.13" && p.modbus_address == "0x400D"));
        assert!(params.iter().any(|p| p.param_id == "F18.30" && p.modbus_address == "0x121E"));
    }
}
