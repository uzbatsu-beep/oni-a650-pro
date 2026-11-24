use a650_core::actor::DriveSession;
use a650_core::modbus::crc16_modbus;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

#[test]
fn transparent_gateway_preserves_rtu_frames_and_checks_state_before_writing() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let gateway = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        // FC03 handshake, one illegal address, then state / frequency / state and FC06.
        let expected = [(3, 0x3000), (3, 0x100), (3, 0x2100), (3, 0x3000), (3, 0x2100), (6, 0x0300)];
        for (fc, address) in expected {
            let mut request = [0; 8];
            stream.read_exact(&mut request).unwrap();
            assert_eq!((request[0], request[1], u16::from_be_bytes([request[2], request[3]])), (7, fc, address));
            assert_eq!(crc16_modbus(&request[..6]).to_le_bytes(), request[6..]);
            assert_eq!(&request[4..6], &[0, 1]);
            let mut reply = if fc == 6 { request[..6].to_vec() } else if address == 0x100 {
                vec![7, 0x83, 2]
            } else { vec![7, 3, 2, 0, 0] };
            reply.extend_from_slice(&crc16_modbus(&reply).to_le_bytes());
            stream.write_all(&reply[..2]).unwrap();
            thread::sleep(Duration::from_millis(2));
            stream.write_all(&reply[2..]).unwrap();
        }
    });
    let mut drive = DriveSession::default();
    let status = drive.connect_ethernet("127.0.0.1", port, 7, 500).unwrap();
    assert!(status.connected && !status.simulated && !status.write_enabled);
    assert_eq!(status.ethernet_endpoint.unwrap().port(), port);
    assert!(drive.write_register_safe_ex(0x0300, 1, None).is_err());
    assert!(drive.read_registers_ex(0x100, 1).is_err());
    assert!(drive.connected);
    drive.guard.read_only = false;
    drive.write_register_safe_ex(0x0300, 1, None).unwrap();
    gateway.join().unwrap();
    assert!(drive.read_registers_ex(0x3000, 1).is_err());
    assert!(!drive.connected && drive.guard.read_only);
    assert!(drive.read_registers_ex(0x3000, 1).is_err());
    assert!(drive.connect_ethernet("unknown", 502, 1, 500).is_err());
    assert!(drive.require_session(status.session_id).is_err());
}

#[test]
fn setup_mode_uses_measured_zero_only_for_parameter_writes() {
    let mut drive = DriveSession::default();
    drive.connect_or_configure(true, String::new(), 9600, "8N2".into(), 1, 400).unwrap();
    drive.guard.read_only = false;
    drive.sim_regs.insert(0x2100, 0);
    for word in [1, 0xffff] {
        drive.sim_regs.insert(0x3000, word);
        assert!(drive.write_register_safe_ex(0x301, 1, None).is_err());
    }
    drive.sim_regs.insert(0x3000, 0);
    drive.write_register_safe_ex(0x301, 1, None).unwrap();
    for (address, value) in [(0x2000, 1), (0x2001, 3000), (0x4, 1), (0x14, 1), (0x8301, 1)] {
        assert!(drive.write_register_safe_ex(address, value, None).is_err());
    }
}
