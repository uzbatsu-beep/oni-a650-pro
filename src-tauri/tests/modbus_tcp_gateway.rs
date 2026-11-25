use a650_core::actor::{DriveSession, EthernetProtocol};
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
        request(&mut stream, 2, 3, 0x2100, 3);
        reply(&mut stream, 2, &[1, 3, 6, 0, 1, 0, 2, 0, 0]);
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
    assert_eq!(first.status_bits, Some(2));
    assert_eq!(first.fault_code, Some(0));
    let second = drive.poll_monitor(true).unwrap();
    assert!(second.scope_sample_ms.unwrap() > first.scope_sample_ms.unwrap());
    assert!(!second.monitor_fallback && !second.write_enabled);
    task.join().unwrap();
}

#[test]
fn unsupported_mbap_block_falls_back_to_single_reads_for_the_session() {
    let (port, task) = gateway(|mut stream| {
        request(&mut stream, 1, 3, 0x3000, 1);
        reply(&mut stream, 1, &[1, 3, 2, 0, 0]);
        request(&mut stream, 2, 3, 0x2100, 3);
        reply(&mut stream, 2, &[1, 0x83, 2]);
        request(&mut stream, 3, 3, 0x3000, 1);
        reply(&mut stream, 3, &[1, 3, 2, 0, 0]);
        request(&mut stream, 4, 3, 0x2100, 1);
        reply(&mut stream, 4, &[1, 3, 2, 0, 4]);
    });
    let mut drive = DriveSession::default();
    drive.connect_ethernet_with_protocol("127.0.0.1", port, 1, 1000, EthernetProtocol::ModbusTcp).unwrap();
    let rejected = drive.poll_monitor(true).unwrap();
    assert!(rejected.connected && rejected.monitor_fallback);
    assert!(rejected.scope_sample_ms.is_none());
    assert!(drive.poll_monitor(true).unwrap().scope_sample_ms.is_some());
    let next = drive.poll_monitor(true).unwrap();
    assert_eq!(next.state_code, Some(4));
    assert!(next.scope_sample_ms.is_none());
    task.join().unwrap();
}
