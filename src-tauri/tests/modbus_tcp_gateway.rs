use a650_core::actor::{DriveSession, EthernetProtocol, MonitorReadMode};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

// The reference client reads 0x3000 and 0x2100 separately, with Unit ID 1.
fn request(stream: &mut TcpStream, tid: u16, fc: u8, address: u16, value: u16) {
    let mut actual = [0; 12];
    stream.read_exact(&mut actual).unwrap();
    let [hi, lo] = tid.to_be_bytes();
    let [a, b] = address.to_be_bytes();
    let [c, d] = value.to_be_bytes();
    assert_eq!(actual, [hi, lo, 0, 0, 0, 6, 1, fc, a, b, c, d]);
}

fn reply(stream: &mut TcpStream, tid: u16, body: &[u8]) {
    let [hi, lo] = tid.to_be_bytes();
    let header = [hi, lo, 0, 0, 0, body.len() as u8];
    stream.write_all(&header[..2]).unwrap();
    thread::sleep(Duration::from_millis(2));
    stream.write_all(&header[2..]).unwrap();
    stream.write_all(body).unwrap();
}

fn gateway(run: impl FnOnce(TcpStream) + Send + 'static) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        run(stream);
    });
    (port, task)
}

#[test]
fn opc_compatible_gateway_reads_and_guarded_write_use_mbap_on_the_wire() {
    let (port, task) = gateway(|mut stream| {
        request(&mut stream, 1, 3, 0x3000, 1);
        reply(&mut stream, 1, &[1, 3, 2, 0, 0]);
        request(&mut stream, 2, 3, 0x2100, 1);
        reply(&mut stream, 2, &[1, 3, 2, 0, 0]);
        request(&mut stream, 3, 3, 0x2103, 1);
        reply(&mut stream, 3, &[1, 0x83, 0x0B]);
        request(&mut stream, 4, 3, 0x2100, 1);
        reply(&mut stream, 4, &[1, 3, 2, 0, 4]);
        request(&mut stream, 5, 6, 0x0301, 150);
        reply(&mut stream, 5, &[1, 6, 3, 1, 0, 150]);
    });
    let mut drive = DriveSession::default();
    let status = drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    assert_eq!(status.ethernet_protocol, Some(EthernetProtocol::ModbusTcp));
    assert!(status.connected && !status.write_enabled);
    assert_eq!(status.last_tx_hex, "00 01 00 00 00 06 01 03 30 00 00 01");
    assert_eq!(status.last_rx_hex, "00 01 00 00 00 05 01 03 02 00 00");
    assert_eq!(drive.read_registers_ex(0x2100, 1).unwrap().values, [0]);
    assert!(drive.read_registers_ex(0x2103, 1).unwrap_err().contains("GatewayTargetFailedToRespond"));
    assert!(drive.connected);
    assert!(drive.write_register_safe_ex(0x0301, 150, None).is_err());
    drive.guard.read_only = false;
    let write = drive.write_register_safe_ex(0x0301, 150, None).unwrap();
    assert_eq!(write.tx_hex, "00 05 00 00 00 06 01 06 03 01 00 96");
    assert_eq!(write.rx_hex, write.tx_hex);
    task.join().unwrap();
}

#[test]
fn incomplete_header_wrong_transaction_protocol_length_or_unit_cannot_confirm_drive() {
    for response in [
        vec![0, 1, 0],
        vec![0, 2, 0, 0, 0, 5, 1, 3, 2, 0, 0],
        vec![0, 1, 0, 2, 0, 5, 1, 3, 2, 0, 0],
        vec![0, 1, 0, 0, 1, 0],
        vec![0, 1, 0, 0, 0, 1],
        vec![0, 1, 0, 0, 0, 5, 2, 3, 2, 0, 0],
        vec![0, 1, 0, 0, 0, 5, 1, 3, 2, 0],
        vec![0, 1, 0, 0, 0, 5, 1, 3, 4, 0, 0],
    ] {
        let (port, task) = gateway(move |mut s| {
            request(&mut s, 1, 3, 0x3000, 1);
            s.write_all(&response).unwrap();
        });
        let mut drive = DriveSession::default();
        assert!(drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).is_err());
        assert!(!drive.connected && drive.guard.read_only);
        task.join().unwrap();
    }
}

#[test]
fn wrong_fc06_echo_relocks_and_no_write_retry_is_sent() {
    let (port, task) = gateway(|mut s| {
        request(&mut s, 1, 3, 0x3000, 1);
        reply(&mut s, 1, &[1, 3, 2, 0, 0]);
        request(&mut s, 2, 3, 0x2100, 1);
        reply(&mut s, 2, &[1, 3, 2, 0, 4]);
        request(&mut s, 3, 6, 0x0301, 150);
        reply(&mut s, 3, &[1, 3, 3, 1, 0, 150]);
        assert_eq!(s.read(&mut [0; 1]).unwrap(), 0);
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    drive.guard.read_only = false;
    assert!(drive.write_register_safe_ex(0x0301, 150, None).is_err());
    assert!(drive.guard.read_only && !drive.connected);
    task.join().unwrap();
}

#[test]
fn grouped_monitor_updates_all_scope_channels_from_one_mbap_reply() {
    let (port, task) = gateway(|mut stream| {
        request(&mut stream, 1, 3, 0x3000, 1);
        reply(&mut stream, 1, &[1, 3, 2, 0, 0]);
        request(&mut stream, 2, 3, 0x2100, 1);
        reply(&mut stream, 2, &[1, 3, 2, 0, 1]);
        for id in [3, 4] {
            request(&mut stream, id, 3, 0x3000, 7);
            reply(&mut stream, id, &[1, 3, 14, 0xec, 0x78, 0x13, 0x88, 1, 124, 0, 42, 0, 0, 0, 0, 2, 32]);
        }
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    let first = drive.poll_monitor(true).unwrap();
    assert_eq!(first.output_freq_hz, Some(-50.0));
    assert_eq!(first.target_freq_hz, Some(50.0));
    assert_eq!(first.output_volt, Some(380));
    assert_eq!(first.dc_bus_volt, Some(544));
    assert_eq!(first.output_current_a, Some(4.2));
    assert_eq!(first.state_code, Some(1));
    assert_eq!(first.status_bits, None);
    assert_eq!(first.fault_code, None);
    let second = drive.poll_monitor(true).unwrap();
    assert!(second.scope_sample_ms.unwrap() > first.scope_sample_ms.unwrap());
    assert!(!second.write_enabled);
    task.join().unwrap();
}

#[test]
fn rejected_block_can_be_changed_to_split_single_and_back_without_reconnect() {
    use MonitorReadMode::{Block4, Block7, Single};
    let (port, task) = gateway(|mut stream| {
        for (tid, address, count, body) in [
            (1, 0x3000, 1, &[1, 3, 2, 0, 0][..]),
            (2, 0x2100, 1, &[1, 0x83, 2]),
            (3, 0x3000, 7, &[1, 0x83, 3]),
            (4, 0x3000, 4, &[1, 3, 8, 0, 100, 0, 100, 1, 124, 0, 42]),
            (5, 0x3006, 1, &[1, 3, 2, 2, 32]),
            (6, 0x3000, 1, &[1, 3, 2, 0, 200]),
            (7, 0x3000, 7, &[1, 3, 14, 0, 200, 0, 200, 1, 124, 0, 42, 0, 0, 0, 0, 2, 32]),
        ] {
            request(&mut stream, tid, 3, address, count);
            reply(&mut stream, tid, body);
        }
    });
    let mut drive = DriveSession::default();
    let session = drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    let rejected = drive.poll_monitor_mode(Block7, 100).unwrap();
    assert!(rejected.connected && rejected.scope_sample_ms.is_none());
    let error = rejected.last_error.unwrap();
    assert!(error.contains("0x3000") && error.contains("регистров 7") && error.contains("0x03"));
    let split = drive.poll_monitor_mode(Block4, 100).unwrap();
    assert_eq!(split.output_freq_hz, Some(1.0));
    assert_eq!(split.output_current_a, Some(4.2));
    assert_eq!(split.dc_bus_volt, Some(544));
    assert!(split.scope_sample_ms.is_some());
    let single = drive.poll_monitor_mode(Single, 100).unwrap();
    assert_eq!(single.output_freq_hz, Some(2.0));
    assert!(drive.poll_monitor_mode(Block7, 350).unwrap().scope_sample_ms.is_some());
    assert!(drive.require_session(session.session_id).is_ok());
    task.join().unwrap();
}

#[test]
fn rejected_diagnostic_preserves_the_successful_scope_reply_and_failed_address() {
    let (port, task) = gateway(|mut stream| {
        request(&mut stream, 1, 3, 0x3000, 1);
        reply(&mut stream, 1, &[1, 3, 2, 0, 0]);
        request(&mut stream, 2, 3, 0x2100, 1);
        reply(&mut stream, 2, &[1, 0x83, 2]);
        request(&mut stream, 3, 3, 0x3000, 7);
        reply(&mut stream, 3, &[1, 3, 14, 0, 0, 0x13, 0x88, 0, 0, 0, 0, 0, 0, 0, 0, 2, 32]);
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    let sample = drive.poll_monitor_mode(MonitorReadMode::Block7, 50).unwrap();
    assert_eq!(sample.dc_bus_volt, Some(544));
    assert!(sample.scope_sample_ms.is_some());
    let error = sample.last_error.unwrap();
    assert!(error.contains("0x2100") && error.contains("регистров 1") && error.contains("0x02"));
    task.join().unwrap();
}

#[test]
fn eeprom_save_reads_live_ram_and_writes_only_the_approved_mirror() {
    let (port, task) = gateway(|mut s| {
        for (tid, address, value) in [
            (1, 0x3000, 0u16), // connect
            (2, 0x0300, 150),  // operator preview
            (3, 0x0300, 150),  // confirm RAM still matches preview
            (4, 0x2100, 4),    // fresh stop state
        ] {
            request(&mut s, tid, 3, address, 1);
            let [hi, lo] = value.to_be_bytes();
            reply(&mut s, tid, &[1, 3, 2, hi, lo]);
        }
        request(&mut s, 5, 6, 0x8300, 150);
        reply(&mut s, 5, &[1, 6, 0x83, 0, 0, 150]);
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    assert_eq!(drive.stage_eeprom_parameter("F03.00").unwrap().raw, 150);
    assert!(drive.persist_eeprom_parameter("F03.00", 150).is_err()); // read-only
    drive.guard.read_only = false;
    assert!(drive.write_register_safe_ex(0x8300, 150, None).is_err());
    assert!(drive.persist_eeprom_parameter("F03.00", 151).is_err());
    let result = drive.persist_eeprom_parameter("F03.00", 150).unwrap();
    assert_eq!(result.tx_hex, result.rx_hex);
    assert!(drive.eeprom_preview().pending.is_empty());
    assert!(drive.persist_eeprom_parameter("F03.00", 150).is_err());
    task.join().unwrap();
}

#[test]
fn eeprom_queue_is_per_drive_and_excludes_commands_read_only_and_unconfirmed_values() {
    let mut drive = DriveSession::default();
    drive.simulated = true;
    drive.guard.read_only = false;
    for code in ["F00.04", "F00.05", "F08.30", "F18.30", "F00.20", "H00.30", "H00.39", "H00.40", "H00.41", "H01.01", "U00.00", "0x2000", "💾"] {
        assert!(drive.stage_eeprom_parameter(code).is_err(), "{code}");
    }
    assert_eq!(drive.tx_count, 0);
    drive.write_register_safe_ex(0x0300, 150, None).unwrap();
    assert_eq!(drive.eeprom_preview().pending[0].raw, 150);
    assert!(!drive.sim_regs.contains_key(&0x8300));
    drive.sim_regs.insert(0x0300, 155); // Edited on the local keypad after preview
    assert!(drive.persist_eeprom_parameter("F03.00", 150).is_err());
    assert!(drive.eeprom_preview().pending.is_empty());
    assert!(!drive.sim_regs.contains_key(&0x8300));
    drive.stage_eeprom_parameter("F03.00").unwrap();
    drive.sim_regs.insert(0x2100, 1);
    assert!(drive.persist_eeprom_parameter("F03.00", 155).is_err());
    assert!(!drive.sim_regs.contains_key(&0x8300));
    drive.sim_regs.insert(0x2100, 4);
    drive.persist_eeprom_parameter("F03.00", 155).unwrap();
    assert_eq!(drive.sim_regs[&0x8300], 155);
    drive.stage_eeprom_parameter("F03.01").unwrap();
    drive.disconnect();
    assert!(drive.eeprom_preview().pending.is_empty());
}

#[test]
fn lost_eeprom_echo_stops_without_an_automatic_repeat() {
    let (port, task) = gateway(|mut s| {
        for (tid, address, value) in [(1, 0x3000, 0), (2, 0x0300, 150), (3, 0x0300, 150), (4, 0x2100, 4)] {
            request(&mut s, tid, 3, address, 1);
            reply(&mut s, tid, &[1, 3, 2, 0, value]);
        }
        request(&mut s, 5, 6, 0x8300, 150);
        s.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(s.read(&mut [0; 1]).unwrap(), 0);
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    drive.stage_eeprom_parameter("F03.00").unwrap();
    drive.guard.read_only = false;
    assert!(drive.persist_eeprom_parameter("F03.00", 150).unwrap_err().contains("outcome unknown"));
    assert!(drive.guard.read_only && !drive.connected);
    let sent = drive.tx_count;
    assert!(drive.persist_eeprom_parameter("F03.00", 150).is_err());
    assert_eq!(drive.tx_count, sent);
    task.join().unwrap();
}

#[test]
fn all_44_writable_h00_settings_can_be_staged_and_persisted_in_simulation() {
    let mut drive = DriveSession::default();
    drive.simulated = true;
    drive.guard.read_only = false;
    let available = drive.eeprom_preview().available;
    let settings: Vec<_> = drive.parameters.iter()
        .filter(|p| p.group == "H00" && p.access == "RW").cloned().collect();
    assert_eq!(settings.len(), 44);
    assert_eq!(available.iter().filter(|id| id.starts_with("H00.")).count(), settings.len());
    for p in settings {
        assert!(available.contains(&p.param_id));
        let index = p.param_id[4..].parse::<u16>().unwrap();
        let ram = 0x4000 + index;
        let eeprom = 0xC000 + index;
        let raw = (p.default_value.parse::<f64>().unwrap() / p.scale.parse::<f64>().unwrap()).round() as u16;
        drive.sim_regs.insert(ram, raw);
        let item = drive.stage_eeprom_parameter(&p.param_id).unwrap();
        assert_eq!((item.ram, item.eeprom, item.raw), (ram, eeprom, raw));
        drive.persist_eeprom_parameter(&p.param_id, raw).unwrap();
        assert_eq!(drive.sim_regs.get(&eeprom), Some(&raw));
    }
    assert!(drive.eeprom_preview().pending.is_empty());
}

#[test]
fn mbap_h00_save_uses_fresh_limits_fresh_state_and_the_studio_eeprom_address() {
    let (port, task) = gateway(|mut s| {
        for (tid, address, value) in [
            (1, 0x3000, 0u16),
            (2, 0x4002, 4500), (3, 0x4003, 1000), (4, 0x0108, 5000), // preview
            (5, 0x4002, 4500), (6, 0x4003, 1000), (7, 0x0108, 4000), (8, 0x2100, 4), // lower maximum
            (9, 0x4002, 4500), (10, 0x4003, 1000), (11, 0x0108, 5000), (12, 0x2100, 4),
        ] {
            request(&mut s, tid, 3, address, 1);
            let [hi, lo] = value.to_be_bytes();
            reply(&mut s, tid, &[1, 3, 2, hi, lo]);
        }
        request(&mut s, 13, 6, 0xC002, 4500);
        reply(&mut s, 13, &[1, 6, 0xC0, 2, 0x11, 0x94]);
        request(&mut s, 14, 3, 0x4003, 1);
        reply(&mut s, 14, &[1, 3, 2, 0x03, 0xE8]);
        request(&mut s, 15, 3, 0x4002, 1);
        reply(&mut s, 15, &[1, 0x83, 0x02]);
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    assert_eq!(drive.stage_eeprom_parameter("H00.02").unwrap().raw, 4500);
    assert!(drive.persist_eeprom_parameter("H00.02", 4500).is_err()); // still locked
    drive.guard.read_only = false;
    assert!(drive.write_register_safe_ex(0xC002, 4500, None).is_err());
    assert!(drive.persist_eeprom_parameter("H00.02", 4500).unwrap_err().contains("outside"));
    let result = drive.persist_eeprom_parameter("H00.02", 4500).unwrap();
    assert_eq!(result.tx_hex, result.rx_hex);
    assert!(drive.stage_eeprom_parameter("H00.03").unwrap_err().contains("H00.02"));
    assert!(drive.eeprom_preview().pending.is_empty());
    assert_eq!(drive.tx_count, 15);
    task.join().unwrap();
}

#[test]
fn normal_h00_ram_writes_queue_only_confirmed_values_and_check_the_other_threshold() {
    let mut drive = DriveSession::default();
    drive.simulated = true;
    drive.guard.read_only = false;
    drive.sim_regs.insert(0x4002, 3500);
    assert!(drive.write_register_safe_ex(0x4003, 3501, None).is_err());
    assert!(drive.eeprom_preview().pending.is_empty());
    drive.write_register_safe_ex(0x4003, 3500, None).unwrap();
    let item = &drive.eeprom_preview().pending[0];
    assert_eq!((&*item.code, item.eeprom, item.raw), ("H00.03", 0xC003, 3500));
    assert!(!drive.sim_regs.contains_key(&0xC003));
    drive.sim_regs.insert(0x4002, 3499);
    assert!(drive.persist_eeprom_parameter("H00.03", 3500).is_err());
    assert!(!drive.sim_regs.contains_key(&0xC003));
}
