//! Thread-safe Drive Controller & Persistent RS-485 / Simulator Actor for ONI A650 / A650M.

use crate::addresses::parameter_addresses;
use crate::catalog::{load_embedded_faults, load_embedded_parameters, FaultRecord, ParameterRecord};
use crate::modbus::{
    ModbusError,
    crc16_modbus, format_hex, parse_read_body, parse_read_response, parse_write_body, parse_write_response, ReadRegistersRequest,
    WriteRegisterRequest,
};
use crate::safety::{persistent_setting, validate_parameter_value, AuditLogEntry, SafetyError, SafetyGuard};
use serde::{Deserialize, Serialize};
use serialport::{ClearBuffer, DataBits, Parity, SerialPort, SerialPortType, StopBits};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerialPortInfoDto {
    pub name: String,
    pub short_label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStatusDto {
    pub ethernet_protocol: Option<EthernetProtocol>,
    pub session_id: u64,
    pub simulated: bool,
    pub connected: bool,
    pub write_enabled: bool,
    pub ethernet_endpoint: Option<SocketAddr>,
    pub port_name: String,
    pub baud_rate: u32,
    pub frame_format: String,
    pub slave_id: u8,
    pub tx_count: u64,
    pub rx_count: u64,
    pub err_count: u64,
    pub last_rtt_ms: u64,
    pub last_tx_hex: String,
    pub last_rx_hex: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveTelemetryDto {
    /// Monotonic time of a fresh U00.00 reply received in this monitor call.
    pub scope_sample_ms: Option<u64>,
    pub connected: bool,
    pub simulated: bool,
    pub write_enabled: bool,
    pub state_code: Option<u16>,
    pub status_bits: Option<u16>,
    pub fault_code: Option<u16>,
    pub warning_code: Option<u16>,
    pub output_freq_hz: Option<f64>,
    /// Raw FC03 holding register 0x3000, preserved for troubleshooting.
    pub output_freq_raw: Option<u16>,
    pub target_freq_hz: Option<f64>,
    pub dc_bus_volt: Option<u16>,
    pub output_volt: Option<u16>,
    pub output_current_a: Option<f64>,
    pub output_current_raw: Option<u16>,
    pub tx_count: u64,
    pub rx_count: u64,
    pub err_count: u64,
    pub last_rtt_ms: u64,
    pub last_tx_hex: String,
    pub last_rx_hex: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterExchangeResultDto {
    pub values: Vec<u16>,
    pub tx_hex: String,
    pub rx_hex: String,
    pub rtt_ms: u64,
}

/// Polling sequence using single-register FC03 requests (count = 1),
/// matching OniDriveStudio v1.2.3.0.exe behavior and avoiding multi-register blocks.
/// U00.00 / 0x3000 is a signed frequency when F15.08 = 0 (factory default).
/// Under F15.08 = 1 the drive reports the absolute frequency, so the
/// sign cannot be recovered from U00.00 alone.
pub fn decode_output_frequency_hz(raw: u16) -> f64 {
    f64::from(raw as i16) / 100.0
}

const TELEMETRY_SINGLE_REG_POLL_ORDER: [u16; 8] = [
    0x3000, // U00.00 Рабочая частота (0.01 Гц)
    0x2100, // Operating state
    0x2102, // Current fault (also read outside the fault state to confirm clearance)
    0x2101, // Drive status word including output direction
    0x3003, // U00.03 Выходной ток (0.1 А)
    0x3006, // U00.06 Напряжение шины DC (1 В)
    0x3002, // U00.02 Выходное напряжение (1 В)
    0x3001, // U00.01 Настройка частоты (0.01 Гц)
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EthernetProtocol {
    #[default]
    RtuOverTcp,
    ModbusTcp,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorReadMode {
    #[default]
    Single,
    Block4,
    Block7,
}

enum DriveLink {
    Serial(Box<dyn SerialPort>),
    Ethernet(TcpStream, EthernetProtocol),
}

impl DriveLink {
    fn exchange(&mut self, request: &[u8], tail: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        match self {
            Self::Serial(port) => {
                let _ = port.clear(ClearBuffer::Input);
                exchange_rtu(port.as_mut(), request, tail, timeout)
            }
            Self::Ethernet(stream, EthernetProtocol::RtuOverTcp) => exchange_rtu(stream, request, tail, timeout),
            Self::Ethernet(stream, EthernetProtocol::ModbusTcp) => exchange_modbus_tcp(stream, request, timeout),
        }
    }
}

fn exchange_rtu<T: Read + Write + ?Sized>(stream: &mut T, request: &[u8], tail: usize, timeout: Duration) -> Result<Vec<u8>, String> {
    stream.write_all(request).and_then(|_| stream.flush()).map_err(|e| e.to_string())?;
    read_modbus_rtu_response(stream, tail, timeout)
}

fn exchange_modbus_tcp(stream: &mut TcpStream, request: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
    stream.write_all(request).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut header = [0; 6];
    read_bytes_with_deadline(stream, &mut header, deadline).map_err(|e| format!("Modbus TCP, заголовок MBAP: {e}"))?;
    if header[..2] != request[..2] || header[2..4] != [0, 0] {
        return Err(format!("Некорректный MBAP (Transaction ID / Protocol ID): {}", format_hex(&header)));
    }
    // Length includes Unit ID and at most 253 PDU bytes. TCP may fragment the reply.
    let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
    if !(2..=254).contains(&length) { return Err(format!("Некорректная длина MBAP: {length}")); }
    let mut frame = header.to_vec();
    frame.resize(header.len() + length, 0);
    read_bytes_with_deadline(stream, &mut frame[header.len()..], deadline)
        .map_err(|e| format!("Modbus TCP, тело ответа: {e}"))?;
    Ok(frame)
}

#[derive(Debug, Clone, Serialize)]
pub struct EepromItem {
    pub code: String,
    pub ram: u16,
    pub eeprom: u16,
    pub raw: u16,
}

#[derive(Debug, Serialize)]
pub struct EepromPreview {
    pub pending: Vec<EepromItem>,
    pub available: Vec<String>,
}

pub struct DriveSession {
    pending_eeprom: BTreeMap<String, EepromItem>,
    session_id: u64,
    next_transaction_id: u16,
    ethernet_protocol: EthernetProtocol,
    pub model: String,
    pub slave_id: u8,
    pub simulated: bool,
    pub connected: bool,
    pub ethernet_endpoint: Option<SocketAddr>,
    pub port_name: String,
    pub baud_rate: u32,
    pub frame_format: String,
    pub timeout_ms: u64,
    pub tx_count: u64,
    pub rx_count: u64,
    pub err_count: u64,
    pub last_rtt_ms: u64,
    pub last_tx_hex: String,
    pub last_rx_hex: String,
    pub last_error: Option<String>,
    pub poll_step_idx: usize,
    monitor_origin: Instant,
    last_monitor_diagnostics: Option<Instant>,
    next_monitor_diagnostic: usize,
    monitor_sample_ttl: Duration,
    pub last_bus_io: Option<Instant>,
    link: Option<DriveLink>,
    // Only validated physical FC03 replies for the active device belong here.
    measured: HashMap<u16, (u16, Instant)>,
    pub guard: SafetyGuard,
    pub sim_regs: HashMap<u16, u16>,
    pub audit_log: Vec<AuditLogEntry>,
    pub parameters: Vec<ParameterRecord>,
    pub faults: Vec<FaultRecord>,
}

impl Default for DriveSession {
    fn default() -> Self {
        let parameters = load_embedded_parameters();
        let faults = load_embedded_faults();
        let mut guard = SafetyGuard::default();
        for p in &parameters {
            if p.access == "RW" {
                if let Ok(addrs) = parameter_addresses(&p.param_id) {
                    // Only RAM register writes are exposed through generic FC06.
                    guard.writable_addresses.insert(addrs.ram);
                }
            }
        }
        let mut sim_regs = HashMap::new();
        // These three frequency settings are interdependent (0.01 Hz per word).
        sim_regs.insert(0x0108, 5000); // F01.08 maximum frequency
        sim_regs.insert(0x4002, 5000); // H00.02 pump start threshold
        sim_regs.insert(0x4003, 500);  // H00.03 pump stop threshold
        sim_regs.insert(0x2100, 4);    // Stopped
        sim_regs.insert(0x2101, 0);    // Forward
        sim_regs.insert(0x2102, 0);    // No fault
        sim_regs.insert(0x2103, 0);
        sim_regs.insert(0x3000, 0);    // U00.00 Рабочая частота (0.01 Гц)
        sim_regs.insert(0x3001, 3000); // U00.01 Задание частоты (30.00 Гц)
        sim_regs.insert(0x3002, 0);    // U00.02 Выходное напряжение (В)
        sim_regs.insert(0x3003, 0);    // U00.03 Выходной ток (0.1 А)
        sim_regs.insert(0x3004, 0);    // U00.04 Выходная мощность (0.1 кВт)
        sim_regs.insert(0x3005, 0);    // U00.05 Скорость вращения (об/мин)
        sim_regs.insert(0x3006, 538);  // U00.06 Напряжение шины DC (В)

        Self {
            session_id: 0,
            next_transaction_id: 1,
            ethernet_protocol: EthernetProtocol::RtuOverTcp,
            model: "A650M".to_string(),
            slave_id: 1,
            simulated: false,
            connected: false,
            ethernet_endpoint: None,
            port_name: "COM3".to_string(),
            baud_rate: 9600,
            frame_format: "1-8-N-2".to_string(),
            timeout_ms: 450,
            tx_count: 0,
            rx_count: 0,
            err_count: 0,
            last_rtt_ms: 0,
            last_tx_hex: String::new(),
            last_rx_hex: String::new(),
            last_error: None,
            poll_step_idx: 0,
            monitor_origin: Instant::now(),
            last_monitor_diagnostics: None,
            next_monitor_diagnostic: 0,
            monitor_sample_ttl: Duration::from_secs(5),
            last_bus_io: None,
            link: None,
            measured: HashMap::new(),
            guard,
            pending_eeprom: BTreeMap::new(),
            sim_regs,
            audit_log: Vec::new(),
            parameters,
            faults,
        }
    }
}

fn parse_serial_format(fmt: &str) -> (Parity, StopBits) {
    match fmt.trim() {
        "1-8-N-2" | "8N2" => (Parity::None, StopBits::Two),
        "1-8-E-1" | "8E1" | "1-8-E-2" => (Parity::Even, StopBits::One),
        "1-8-O-1" | "8O1" => (Parity::Odd, StopBits::One),
        "1-8-N-1" | "8N1" => (Parity::None, StopBits::One),
        _ => (Parity::None, StopBits::Two),
    }
}

/// Reads an exact number of bytes from `port` up to `deadline`, tolerating chunked USB-UART arrivals.
fn read_bytes_with_deadline<R: Read + ?Sized>(
    port: &mut R,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<(), std::io::Error> {
    let mut offset = 0usize;
    while offset < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("таймаут чтения ({} из {} байт получено)", offset, buf.len()),
            ));
        }
        match port.read(&mut buf[offset..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                offset += n;
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("таймаут ответа ПЧ ({} из {} байт получено)", offset, buf.len()),
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Smart Modbus RTU response reader:
/// Reads the 3-byte header `[slave, fc, byte2]` first so 5-byte Modbus exception frames
/// (`0x83` / `0x86`) are caught immediately instead of hanging `read_exact` waiting for `5 + 2*count` bytes!
fn read_modbus_rtu_response<R: Read + ?Sized>(
    port: &mut R,
    expected_tail_after_header: usize,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut hdr = [0u8; 3];
    read_bytes_with_deadline(port, &mut hdr, deadline).map_err(|e| e.to_string())?;

    // If highest bit of function code is set (0x83, 0x86), this is a 5-byte Modbus exception frame
    let tail_len = if (hdr[1] & 0x80) != 0 {
        2usize
    } else {
        expected_tail_after_header
    };

    let mut tail = vec![0u8; tail_len];
    read_bytes_with_deadline(port, &mut tail, deadline).map_err(|e| e.to_string())?;

    let mut full = Vec::with_capacity(3 + tail_len);
    full.extend_from_slice(&hdr);
    full.extend_from_slice(&tail);
    Ok(full)
}

impl DriveSession {
    pub fn require_session(&self, session_id: u64) -> Result<(), String> {
        if session_id != self.session_id {
            return Err("Сеанс подключения изменился. Повторите операцию для выбранного ПЧ.".into());
        }
        Ok(())
    }

    fn reset_measurements(&mut self) {
        self.session_id += 1;
        self.pending_eeprom.clear();
        // Connection changes always revoke the operator's write unlock.
        self.guard.read_only = true;
        self.measured.clear();
        self.last_tx_hex.clear();
        self.last_rx_hex.clear();
        self.last_rtt_ms = 0;
        self.connected = false;
        self.poll_step_idx = 0;
        self.monitor_origin = Instant::now();
        self.last_monitor_diagnostics = None;
        self.next_monitor_diagnostic = 0;
        self.monitor_sample_ttl = Duration::from_secs(5);
    }

    fn sample(&self, address: u16) -> Option<u16> {
        if self.simulated {
            return self.sim_regs.get(&address).copied();
        }
        let (value, received) = self.measured.get(&address)?;
        (self.connected && received.elapsed() <= self.monitor_sample_ttl).then_some(*value)
    }

    pub fn record_audit(&mut self, op: &str, details: serde_json::Value) {
        let entry = AuditLogEntry::new(op, self.slave_id, &self.model, details);
        self.audit_log.insert(0, entry);
        if self.audit_log.len() > 200 {
            self.audit_log.pop();
        }
    }

    pub fn status_dto(&self) -> ConnectionStatusDto {
        ConnectionStatusDto {
            session_id: self.session_id,
            simulated: self.simulated,
            connected: self.connected,
            write_enabled: !self.guard.read_only,
            ethernet_endpoint: self.ethernet_endpoint,
            ethernet_protocol: self.ethernet_endpoint.map(|_| self.ethernet_protocol),
            port_name: self.port_name.clone(),
            baud_rate: self.baud_rate,
            frame_format: self.frame_format.clone(),
            slave_id: self.slave_id,
            tx_count: self.tx_count,
            rx_count: self.rx_count,
            err_count: self.err_count,
            last_rtt_ms: self.last_rtt_ms,
            last_tx_hex: self.last_tx_hex.clone(),
            last_rx_hex: self.last_rx_hex.clone(),
            last_error: self.last_error.clone(),
        }
    }

    fn ensure_port_open(&mut self) -> Result<(), String> {
        if self.link.is_some() {
            return Ok(());
        }
        if self.ethernet_endpoint.is_some() {
            return Err("Соединение TCP закрыто. Подключитесь к шлюзу повторно.".into());
        }
        let (parity, stop_bits) = parse_serial_format(&self.frame_format);
        let port = serialport::new(&self.port_name, self.baud_rate)
            .data_bits(DataBits::Eight)
            .parity(parity)
            .stop_bits(stop_bits)
            .timeout(Duration::from_millis(40))
            .open()
            .map_err(|e| {
                format!(
                    "Не удалось открыть {} ({} бод, {}): {}",
                    self.port_name, self.baud_rate, self.frame_format, e
                )
            })?;
        self.link = Some(DriveLink::Serial(port));
        // Allow USB-RS485 transceiver (CP210x / CH340) lines to settle after open
        thread::sleep(Duration::from_millis(25));
        Ok(())
    }

    fn enforce_interframe_guard(&mut self) {
        if let Some(prev) = self.last_bus_io {
            let elapsed = prev.elapsed();
            let guard_dur = Duration::from_millis(18);
            if elapsed < guard_dur {
                thread::sleep(guard_dur - elapsed);
            }
        }
    }

    pub fn connect_or_configure(
        &mut self,
        simulated: bool,
        port_name: String,
        baud_rate: u32,
        frame_format: String,
        slave_id: u8,
        timeout_ms: u64,
    ) -> Result<ConnectionStatusDto, String> {
        // Drop previous serial handle so new port/baud/parity takes effect cleanly
        self.link = None;
        self.reset_measurements();
        self.ethernet_endpoint = None;
        self.simulated = simulated;
        self.port_name = port_name.trim().to_string();
        self.baud_rate = baud_rate;
        self.frame_format = frame_format;
        self.slave_id = slave_id;
        self.timeout_ms = timeout_ms.clamp(150, 2000);
        self.last_error = None;

        if self.simulated {
            self.connected = false;
            self.record_audit(
                "switch_to_simulator",
                serde_json::json!({
                    "simulated": true,
                    "slave_id": self.slave_id
                }),
            );
            return Ok(self.status_dto());
        }

        self.ensure_port_open()?;
        self.verify_connection()
    }

    pub fn connect_ethernet(&mut self, ip: &str, port: u16, slave: u8, timeout_ms: u64) -> Result<ConnectionStatusDto, String> {
        self.connect_ethernet_with_protocol(ip, port, slave, timeout_ms, EthernetProtocol::RtuOverTcp)
    }

    pub fn connect_ethernet_with_protocol(&mut self, ip: &str, port: u16, slave: u8, timeout_ms: u64, protocol: EthernetProtocol) -> Result<ConnectionStatusDto, String> {
        self.disconnect();
        let ip: IpAddr = ip.trim().parse().map_err(|_| "Введите IP-адрес шлюза.")?;
        if port == 0 || !(1..=247).contains(&slave) {
            return Err("Порт должен быть 1–65535, адрес ПЧ 1–247.".into());
        }
        let endpoint = SocketAddr::new(ip, port);
        self.ethernet_endpoint = Some(endpoint);
        self.ethernet_protocol = protocol;
        self.next_transaction_id = 1;
        self.port_name = endpoint.to_string();
        self.slave_id = slave;
        self.timeout_ms = timeout_ms.clamp(150, 2000);
        let timeout = Duration::from_millis(self.timeout_ms);
        let stream = TcpStream::connect_timeout(&endpoint, timeout)
            .map_err(|e| format!("Шлюз {endpoint}: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_millis(40))).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        self.link = Some(DriveLink::Ethernet(stream, protocol));
        self.verify_connection()
    }

    fn uses_mbap(&self) -> bool {
        self.ethernet_endpoint.is_some() && self.ethernet_protocol == EthernetProtocol::ModbusTcp
    }

    fn wire_request(&mut self, rtu: [u8; 8]) -> Vec<u8> {
        if !self.uses_mbap() { return rtu.to_vec(); }
        let id = self.next_transaction_id;
        self.next_transaction_id = id.wrapping_add(1);
        // Both supported requests have a six-byte Unit ID + PDU.
        let mut request = Vec::with_capacity(12);
        request.extend_from_slice(&id.to_be_bytes());
        request.extend_from_slice(&[0, 0, 0, 6]);
        request.extend_from_slice(&rtu[..6]);
        request
    }

    fn verify_connection(&mut self) -> Result<ConnectionStatusDto, String> {
        match self.read_registers_ex(0x3000, 1) {
            Ok(res) => {
                self.connected = true;
                self.last_error = None;
                self.record_audit(
                    "connect_rtu_ok",
                    serde_json::json!({
                        "port": self.port_name,
                        "baud": self.baud_rate,
                        "format": self.frame_format,
                        "ethernet_protocol": self.ethernet_endpoint.map(|_| self.ethernet_protocol),
                        "slave_id": self.slave_id,
                        "rtt_ms": res.rtt_ms,
                        "tx_hex": res.tx_hex,
                        "rx_hex": res.rx_hex
                    }),
                );
                Ok(self.status_dto())
            }
            Err(err) => {
                self.connected = false;
                self.link = None;
                self.last_error = Some(err.clone());
                self.record_audit(
                    "connect_rtu_failed",
                    serde_json::json!({
                        "port": self.port_name,
                        "baud": self.baud_rate,
                        "format": self.frame_format,
                        "ethernet_protocol": self.ethernet_endpoint.map(|_| self.ethernet_protocol),
                        "slave_id": self.slave_id,
                        "error": err
                    }),
                );
                Err(err)
            }
        }
    }

    pub fn disconnect(&mut self) -> ConnectionStatusDto {
        self.link = None;
        self.reset_measurements();
        self.ethernet_endpoint = None;
        self.simulated = false;
        self.last_error = None;
        self.record_audit("disconnect", serde_json::json!({}));
        self.status_dto()
    }

    pub fn write_register_safe(
        &mut self,
        address: u16,
        value: u16,
        stop_only_code: Option<&str>,
    ) -> Result<String, String> {
        let res = self.write_register_safe_ex(address, value, stop_only_code)?;
        Ok(res.tx_hex)
    }

    fn reject_write<T>(&mut self, address: u16, value: u16, reason: String) -> Result<T, String> {
        self.record_audit("write_rejected", serde_json::json!({
            "address": format!("0x{:04X}", address),
            "raw_value": value,
            "reason": reason,
            "simulated": self.simulated,
        }));
        Err(reason)
    }

    pub fn write_register_safe_ex(
        &mut self,
        address: u16,
        value: u16,
        _stop_only_code: Option<&str>, // Legacy UI hint is never trusted.
    ) -> Result<RegisterExchangeResultDto, String> {
        self.authorize_ram_write(address, value)?;
        let result = self.exchange_fc06(address, value)?;
        if let Some(item) = self.parameters.iter().filter(|p| persistent_setting(p)).find_map(|p| {
            let pair = parameter_addresses(&p.param_id).ok()?;
            if pair.ram != address { return None; }
            Some(EepromItem { code: p.param_id.clone(), ram: address, eeprom: pair.eeprom?, raw: value })
        }) {
            self.pending_eeprom.insert(item.code.clone(), item);
        }
        Ok(result)
    }

    pub fn eeprom_preview(&self) -> EepromPreview {
        EepromPreview {
            pending: self.pending_eeprom.values().cloned().collect(),
            available: self.parameters.iter().filter(|p| persistent_setting(p))
                .map(|p| p.param_id.clone()).collect(),
        }
    }

    pub fn stage_eeprom_parameter(&mut self, code: &str) -> Result<EepromItem, String> {
        if !self.simulated && !self.connected { return Err("Подключите ПЧ.".into()); }
        let index = self.parameters.iter().position(|p| p.param_id == code && persistent_setting(p))
            .ok_or("Для этого параметра сохранение EEPROM недоступно.")?;
        let pair = parameter_addresses(&self.parameters[index].param_id).map_err(|e| e.to_string())?;
        let eeprom = pair.eeprom.ok_or("Адрес EEPROM отсутствует.")?;
        let raw = self.read_registers_ex(pair.ram, 1)?.values[0];
        let parameter = self.parameter_with_live_limits(index)?;
        validate_parameter_value(&parameter, raw).map_err(|e| e.to_string())?;
        let item = EepromItem { code: code.to_owned(), ram: pair.ram, eeprom, raw };
        self.pending_eeprom.insert(code.to_owned(), item.clone());
        Ok(item)
    }

    /// Only a value previewed in this session can reach a catalog-derived EEPROM address.
    pub fn persist_eeprom_parameter(&mut self, code: &str, expected_raw: u16) -> Result<RegisterExchangeResultDto, String> {
        let item = self.pending_eeprom.get(code).cloned().ok_or("Сначала прочитайте параметр в список сохранения.")?;
        if item.raw != expected_raw { return Err("Предпросмотр устарел. Обновите список.".into()); }
        self.guard.check_address(self.slave_id, item.ram).map_err(|e| e.to_string())?;
        if !self.simulated && !self.connected { return Err("Подключите ПЧ.".into()); }
        let raw = self.read_registers_ex(item.ram, 1)?.values[0];
        if raw != item.raw {
            self.pending_eeprom.remove(code);
            return Err(format!("{code}: RAM изменилась. Повторно добавьте параметр и подтвердите новое значение."));
        }
        self.authorize_ram_write(item.ram, raw)?;
        let result = self.exchange_fc06(item.eeprom, raw)?;
        self.pending_eeprom.remove(code);
        self.record_audit("persist_eeprom_parameter", serde_json::json!({
            "parameter": code, "ram": item.ram, "eeprom": item.eeprom, "raw": raw, "simulated": self.simulated,
        }));
        Ok(result)
    }

    /// Resolve only the named pump-frequency references in the A650M catalog.
    /// Other symbolic limits are still rejected by validate_parameter_value.
    fn parameter_with_live_limits(&mut self, index: usize) -> Result<ParameterRecord, String> {
        let mut parameter = self.parameters[index].clone();
        if matches!(parameter.param_id.as_str(), "H00.02" | "H00.03") {
            for bound in [&mut parameter.min_value, &mut parameter.max_value] {
                if matches!(bound.as_str(), "H00.02" | "H00.03" | "F01.08") {
                    let address = parameter_addresses(bound).map_err(|e| e.to_string())?.ram;
                    let raw = self.read_registers_ex(address, 1)
                        .map_err(|e| format!("Не удалось прочитать предел {bound}: {e}"))?.values[0];
                    // All three referenced registers contain unsigned hundredths of a Hz.
                    *bound = (f64::from(raw) / 100.0).to_string();
                }
            }
        }
        Ok(parameter)
    }

    fn authorize_ram_write(&mut self, address: u16, value: u16) -> Result<(), String> {
        // Reject forbidden writes before any physical bus I/O.
        if let Err(err) = self.guard.check_address(self.slave_id, address) {
            return self.reject_write(address, value, err.to_string());
        }

        if !self.simulated && !self.connected {
            self.guard.read_only = true;
            return self.reject_write(address, value,
                "FC06 denied: physical connection not verified; SafetyGuard re-locked".to_string());
        }

        let index = self.parameters.iter().position(|p| p.access == "RW"
            && parameter_addresses(&p.param_id).ok().map(|a| a.ram) == Some(address));
        let parameter = match index.map(|i| self.parameter_with_live_limits(i)).transpose() {
            Ok(parameter) => parameter,
            Err(error) => return self.reject_write(address, value, error),
        };

        // Read the state after all live bounds and immediately before FC06 in the locked session.
        let fresh_state = if self.simulated {
            self.sim_regs.get(&0x2100).copied()
        } else {
            match self.read_registers_ex(0x2100, 1) {
                Ok(result) => result.values.first().copied(),
                Err(err) => {
                    self.guard.read_only = true;
                    return self.reject_write(address, value, format!(
                        "FC06 denied: fresh FC03 state 0x2100 unavailable ({}); SafetyGuard re-locked", err
                    ));
                }
            }
        };

        // State 0 means parameter setup. Confirm zero output frequency and
        // re-read the state after it, in the same locked session as FC06.
        let mut verified_state = fresh_state;
        let mut setup_frequency = None;
        if address != 0x2000 && address != 0x2001 && fresh_state == Some(0) {
            let verification = self.read_registers_ex(0x3000, 1).and_then(|frequency| {
                self.read_registers_ex(0x2100, 1).map(|state| (frequency.values[0], state.values[0]))
            });
            match verification {
                Ok((frequency, 0)) => setup_frequency = Some(frequency),
                Ok(_) => verified_state = None,
                Err(error) => {
                    self.guard.read_only = true;
                    return self.reject_write(address, value, error);
                }
            }
        }

        if let Err(err) = self.guard.check_write_with_frequency(self.slave_id, address, value, verified_state, parameter.as_ref(), setup_frequency) {
            return self.reject_write(address, value, err.to_string());
        }

        Ok(())
    }

    fn exchange_fc06(&mut self, address: u16, value: u16) -> Result<RegisterExchangeResultDto, String> {
        let req = WriteRegisterRequest::new(self.slave_id, address, value)
            .map_err(|e| e.to_string())?;
        let tx_bytes = self.wire_request(req.to_bytes());
        let tx_hex = format_hex(&tx_bytes);
        self.tx_count += 1;
        self.last_tx_hex = tx_hex.clone();
        let t0 = Instant::now();

        let rx_hex = if self.simulated {
            if address == 0x2000 {
                match value {
                    1 => {
                        self.sim_regs.insert(0x2100, 1);
                        self.sim_regs.insert(0x2101, 0);
                        let set_f = self.sim_regs.get(&0x3001).copied().unwrap_or(3000);
                        self.sim_regs.insert(0x3000, set_f);
                        self.sim_regs.insert(0x3002, 380);
                        self.sim_regs.insert(0x3003, 125);
                    }
                    2 => {
                        self.sim_regs.insert(0x2100, 1);
                        self.sim_regs.insert(0x2101, 0b0010);
                    }
                    5 | 6 => {
                        self.sim_regs.insert(0x2100, 4);
                        self.sim_regs.insert(0x3000, 0);
                        self.sim_regs.insert(0x3002, 0);
                        self.sim_regs.insert(0x3003, 0);
                    }
                    7 => {
                        self.sim_regs.insert(0x2102, 0);
                        self.sim_regs.insert(0x2100, 4);
                    }
                    _ => {}
                }
            } else if address == 0x2001 {
                self.sim_regs.insert(0x3001, value);
                if self.sim_regs.get(&0x2100).copied().unwrap_or(4) == 1 {
                    self.sim_regs.insert(0x3000, value);
                }
            } else {
                self.sim_regs.insert(address, value);
            }
            parse_write_response(req, &tx_bytes).map_err(|e| e.to_string())?;
            self.rx_count += 1;
            self.last_rx_hex = tx_hex.clone();
            tx_hex.clone()
        } else {
            if let Err(e) = self.ensure_port_open() {
                self.err_count += 1;
                self.connected = false;
                self.guard.read_only = true;
                self.last_error = Some(e.clone());
                return Err(e);
            }
            self.enforce_interframe_guard();
            let timeout = Duration::from_millis(self.timeout_ms.max(250));

            let io_res = self.link.as_mut().ok_or("Транспорт не подключён")?
                .exchange(&tx_bytes, 5, timeout);

            self.last_bus_io = Some(Instant::now());

            let rx_buf = match io_res {
                Ok(buf) => buf,
                Err(error) => {
                    self.err_count += 1;
                    self.link = None;
                    self.connected = false;
                    self.measured.clear();
                    self.guard.read_only = true;
                    let msg = format!("{}: {error}", SafetyError::WriteOutcomeUnknown { address, value });
                    self.last_error = Some(msg.clone());
                    return self.reject_write(address, value, msg);
                }
            };

            let rx_s = format_hex(&rx_buf);
            self.last_rx_hex = rx_s.clone();
            let echo = if self.uses_mbap() { parse_write_body(req, &rx_buf[6..]) }
                else { parse_write_response(req, &rx_buf) };
            if let Err(err) = echo {
                self.err_count += 1;
                self.connected = false;
                self.link = None;
                self.measured.clear();
                self.guard.read_only = true;
                let msg = format!("FC06 echo unverified on 0x{:04X}: {} (RX: {}); Read-Only enabled", address, err, rx_s);
                self.last_error = Some(msg.clone());
                return self.reject_write(address, value, msg);
            }
            self.rx_count += 1;
            self.connected = true;
            self.last_error = None;
            rx_s
        };

        let rtt_ms = t0.elapsed().as_millis() as u64;
        self.last_rtt_ms = rtt_ms;

        self.record_audit(
            "write_register_fc06",
            serde_json::json!({
                "address": format!("0x{:04X}", address),
                "raw_value": value,
                "tx_hex": tx_hex,
                "rx_hex": rx_hex,
                "rtt_ms": rtt_ms,
                "simulated": self.simulated,
            }),
        );

        Ok(RegisterExchangeResultDto {
            values: vec![value],
            tx_hex,
            rx_hex,
            rtt_ms,
        })
    }

    pub fn read_registers(&mut self, address: u16, count: u16) -> Result<(Vec<u16>, String), String> {
        let res = self.read_registers_ex(address, count)?;
        Ok((res.values, res.tx_hex))
    }

    pub fn read_registers_ex(
        &mut self,
        address: u16,
        count: u16,
    ) -> Result<RegisterExchangeResultDto, String> {
        let req = ReadRegistersRequest::new(self.slave_id, address, count)
            .map_err(|e| e.to_string())?;
        let tx_bytes = self.wire_request(req.to_bytes());
        let tx_hex = format_hex(&tx_bytes);
        self.tx_count += 1;
        self.last_tx_hex = tx_hex.clone();
        let t0 = Instant::now();

        if self.simulated {
            let mut vals = Vec::with_capacity(count as usize);
            let mut payload = vec![self.slave_id, 0x03, (count * 2) as u8];
            for offset in 0..count {
                let v = self.sim_regs.get(&(address + offset)).copied().unwrap_or(0);
                vals.push(v);
                payload.push((v >> 8) as u8);
                payload.push((v & 0xFF) as u8);
            }
            let crc = crc16_modbus(&payload);
            payload.push((crc & 0xFF) as u8);
            payload.push((crc >> 8) as u8);
            let rx_hex = format_hex(&payload);
            self.rx_count += 1;
            self.last_rx_hex = rx_hex.clone();
            let rtt_ms = t0.elapsed().as_millis() as u64;
            self.last_rtt_ms = rtt_ms;
            Ok(RegisterExchangeResultDto {
                values: vals,
                tx_hex,
                rx_hex,
                rtt_ms,
            })
        } else {
            if let Err(e) = self.ensure_port_open() {
                self.err_count += 1;
                self.connected = false;
                self.link = None;
                self.measured.clear();
                self.guard.read_only = true;
                self.last_error = Some(e.clone());
                return Err(e);
            }
            self.enforce_interframe_guard();
            let timeout = Duration::from_millis(self.timeout_ms.max(200));
            let expected_tail = (count as usize) * 2 + 2;

            let io_res = self.link.as_mut().ok_or("Транспорт не подключён")?
                .exchange(&tx_bytes, expected_tail, timeout);

            self.last_bus_io = Some(Instant::now());

            let rx_buf = match io_res {
                Ok(buf) => buf,
                Err(e) => {
                    self.err_count += 1;
                    self.connected = false;
                    self.measured.clear();
                    self.guard.read_only = true;
                    // Re-open on next attempt if OS serial handle glitched
                    self.link = None;
                    let msg = format!(
                        "Ошибка чтения ПЧ (ID={}, рег. 0x{:04X}, {} шт.) на {}: {}",
                        self.slave_id, address, count, self.port_name, e
                    );
                    self.last_error = Some(msg.clone());
                    return Err(msg);
                }
            };

            let rx_hex = format_hex(&rx_buf);
            self.last_rx_hex = rx_hex.clone();
            let vals = self.accept_reading(address, count, &rx_buf).map_err(|e| {
                self.err_count += 1;
                let msg = format!("FC03 0x{address:04X}, регистров {count}: {e} (RX: {rx_hex})");
                self.last_error = Some(msg.clone());
                msg
            })?;
            self.rx_count += 1;
            self.last_error = None;
            let rtt_ms = t0.elapsed().as_millis() as u64;
            self.last_rtt_ms = rtt_ms;
            Ok(RegisterExchangeResultDto {
                values: vals,
                tx_hex,
                rx_hex,
                rtt_ms,
            })
        }
    }

    fn accept_reading(&mut self, address: u16, count: u16, frame: &[u8]) -> Result<Vec<u16>, ModbusError> {
        let decoded = if self.uses_mbap() { parse_read_body(self.slave_id, count, &frame[6..]) }
            else { parse_read_response(self.slave_id, count, frame) };
        let values = decoded.inspect_err(|err| {
            if matches!(err, ModbusError::DriveException { .. }) {
                self.measured.retain(|addr, _| !addr.checked_sub(address).is_some_and(|n| n < count));
            } else {
                self.connected = false;
                self.link = None;
                self.measured.clear();
                self.guard.read_only = true;
            }
        })?;
        let received = Instant::now();
        for (index, &value) in values.iter().enumerate() {
            if let Some(addr) = address.checked_add(index as u16) {
                // A fault transition invalidates the code from the previous operating state.
                if addr == 0x2100 && self.sample(addr).map(|old| old == 6) != Some(value == 6) {
                    self.measured.remove(&0x2102);
                }
                if TELEMETRY_SINGLE_REG_POLL_ORDER.contains(&addr) || addr == 0x2103 {
                    self.measured.insert(addr, (value, received));
                }
            }
        }
        self.connected = true;
        Ok(values)
    }

    /// Polls drive telemetry using single-register requests (`count = 1`) in round-robin order.
    /// Optional warning word 0x2103 is not required by the dashboard.
    pub fn poll_telemetry(&mut self) -> Result<DriveTelemetryDto, String> {
        self.poll_monitor(false)
    }

    pub fn poll_monitor(&mut self, grouped: bool) -> Result<DriveTelemetryDto, String> {
        let mode = if grouped { MonitorReadMode::Block7 } else { MonitorReadMode::Single };
        self.poll_monitor_mode(mode, 350)
    }

    pub fn poll_monitor_mode(&mut self, requested: MonitorReadMode, period_ms: u64) -> Result<DriveTelemetryDto, String> {
        if self.simulated {
            return Ok(self.telemetry_snapshot());
        }

        let (mode, period_ms) = if self.uses_mbap() {
            (requested, period_ms.clamp(50, 1000))
        } else {
            (MonitorReadMode::Single, 350)
        };
        // One full single-register sweep can take eight seconds at the slowest setting.
        self.monitor_sample_ttl = if mode == MonitorReadMode::Single {
            Duration::from_millis((period_ms * TELEMETRY_SINGLE_REG_POLL_ORDER.len() as u64 + 1500).max(5000))
        } else {
            Duration::from_secs(5)
        };
        let mut diagnostic_error = None;
        if mode != MonitorReadMode::Single &&
            self.last_monitor_diagnostics.map_or(true, |t| t.elapsed() >= Duration::from_millis(333)) {
            const DIAGNOSTICS: [u16; 3] = [0x2100, 0x2102, 0x2101];
            let address = DIAGNOSTICS[self.next_monitor_diagnostic];
            self.next_monitor_diagnostic = (self.next_monitor_diagnostic + 1) % DIAGNOSTICS.len();
            self.last_monitor_diagnostics = Some(Instant::now());
            diagnostic_error = self.read_registers_ex(address, 1).err();
            if !self.connected {
                return Ok(self.telemetry_snapshot());
            }
        }

        let (address, count) = match mode {
            MonitorReadMode::Single => {
                let address = TELEMETRY_SINGLE_REG_POLL_ORDER[self.poll_step_idx % TELEMETRY_SINGLE_REG_POLL_ORDER.len()];
                self.poll_step_idx = (self.poll_step_idx + 1) % TELEMETRY_SINGLE_REG_POLL_ORDER.len();
                (address, 1)
            }
            MonitorReadMode::Block4 => (0x3000, 4),
            MonitorReadMode::Block7 => (0x3000, 7),
        };
        let fresh_scope = self.read_registers_ex(address, count).is_ok() && address == 0x3000;
        if fresh_scope && mode == MonitorReadMode::Block4 {
            // U00.06 is outside the four-word scope block.
            let _ = self.read_registers_ex(0x3006, 1);
        }
        let mut snapshot = self.telemetry_snapshot();
        snapshot.last_error = snapshot.last_error.or(diagnostic_error);
        if fresh_scope && snapshot.connected {
            snapshot.scope_sample_ms = self.measured.get(&address)
                .map(|(_, received)| received.duration_since(self.monitor_origin).as_millis() as u64);
        }
        Ok(snapshot)
    }

    fn telemetry_snapshot(&self) -> DriveTelemetryDto {
        let state_code = self.sample(0x2100);
        let status_bits = self.sample(0x2101);
        let fault_code = self.sample(0x2102);
        let warning_code = self.sample(0x2103);
        let output_freq_raw = self.sample(0x3000);
        let output_freq_hz = output_freq_raw.map(decode_output_frequency_hz);
        let target_freq_hz = self.sample(0x3001).map(|v| f64::from(v) / 100.0);
        let output_volt = self.sample(0x3002);
        let output_current_raw = self.sample(0x3003);
        let output_current_a = output_current_raw.map(|v| f64::from(v) / 10.0);
        let dc_bus_volt = self.sample(0x3006);

        DriveTelemetryDto {
            scope_sample_ms: None,
            connected: self.connected,
            simulated: self.simulated,
            write_enabled: !self.guard.read_only,
            state_code,
            status_bits,
            fault_code,
            warning_code,
            output_freq_hz,
            output_freq_raw,
            target_freq_hz,
            dc_bus_volt,
            output_volt,
            output_current_a,
            output_current_raw,
            tx_count: self.tx_count,
            rx_count: self.rx_count,
            err_count: self.err_count,
            last_rtt_ms: self.last_rtt_ms,
            last_tx_hex: self.last_tx_hex.clone(),
            last_rx_hex: self.last_rx_hex.clone(),
            last_error: self.last_error.clone(),
        }
    }

    pub fn auto_scan_port(
        &mut self,
        port_name: String,
        slave_id: u8,
    ) -> Result<ConnectionStatusDto, String> {
        self.reset_measurements();
        self.ethernet_endpoint = None;
        let bauds = [9600u32, 19200, 38400, 4800, 57600, 115200];
        let formats = ["1-8-N-2", "1-8-N-1", "1-8-E-1", "1-8-O-1"];
        self.link = None;
        self.port_name = port_name.trim().to_string();
        self.slave_id = slave_id;
        self.simulated = false;
        self.timeout_ms = 220;

        for &b in &bauds {
            for &fmt in &formats {
                self.link = None;
                self.baud_rate = b;
                self.frame_format = fmt.to_string();
                if let Ok(res) = self.read_registers_ex(0x3000, 1) {
                    self.timeout_ms = 450;
                    self.connected = true;
                    self.last_error = None;
                    self.record_audit(
                        "autoscan_found_drive",
                        serde_json::json!({
                            "port": self.port_name,
                            "baud": b,
                            "format": fmt,
                            "slave_id": slave_id,
                            "rtt_ms": res.rtt_ms,
                            "rx_hex": res.rx_hex
                        }),
                    );
                    return Ok(self.status_dto());
                }
            }
        }

        self.timeout_ms = 450;
        self.connected = false;
        self.link = None;
        let msg = format!(
            "ПЧ (Slave {}) на порту {} не ответил ни на одной комбинации (9600..115200 бод, 8N2/8N1/8E1/8O1).",
            slave_id, self.port_name
        );
        self.last_error = Some(msg.clone());
        Err(msg)
    }
}

pub struct SharedDriveState(pub Mutex<DriveSession>);

pub fn enumerate_ports() -> Vec<SerialPortInfoDto> {
    match serialport::available_ports() {
        Ok(ports) => ports
            .into_iter()
            .map(|p| {
                let (chip_short, full_desc) = match p.port_type {
                    SerialPortType::UsbPort(info) => {
                        let prod = info.product.clone().unwrap_or_default();
                        let mfg = info.manufacturer.clone().unwrap_or_default();
                        let short = if prod.contains("CP210") {
                            "CP210x USB".to_string()
                        } else if prod.contains("CH340") || prod.contains("CH341") {
                            "CH340 USB".to_string()
                        } else if prod.contains("FT232") || mfg.contains("FTDI") {
                            "FTDI USB".to_string()
                        } else if !mfg.is_empty() {
                            mfg.chars().take(14).collect()
                        } else {
                            format!("USB {:04X}:{:04X}", info.vid, info.pid)
                        };
                        let full = if !prod.is_empty() {
                            prod
                        } else {
                            format!("USB {:04X}:{:04X}", info.vid, info.pid)
                        };
                        (short, full)
                    }
                    SerialPortType::PciPort => ("PCI COM".to_string(), "Встроенный порт PCI".to_string()),
                    SerialPortType::BluetoothPort => ("BT COM".to_string(), "Bluetooth SPP".to_string()),
                    SerialPortType::Unknown => ("RS-485".to_string(), "Стандартный COM-порт".to_string()),
                };
                SerialPortInfoDto {
                    short_label: format!("{} · {}", p.port_name, chip_short),
                    name: p.port_name,
                    description: full_desc,
                }
            })
            .collect(),
        Err(_) => vec![],
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    fn response(payload: &[u8]) -> Vec<u8> {
        let mut frame = payload.to_vec();
        frame.extend_from_slice(&crc16_modbus(payload).to_le_bytes());
        frame
    }

    #[test]
    fn reconnect_and_disconnect_revoke_prior_session_and_write_permission() {
        let mut drive = DriveSession::default();
        let first = drive.connect_or_configure(true, "COM7".into(), 9600, "8N2".into(), 1, 400).unwrap();
        drive.guard.read_only = false;
        let second = drive.connect_or_configure(true, "COM7".into(), 9600, "8N2".into(), 2, 400).unwrap();
        assert!(drive.require_session(first.session_id).is_err());
        assert!(drive.require_session(second.session_id).is_ok());
        assert!(!second.write_enabled);
        drive.disconnect();
        assert!(drive.require_session(second.session_id).is_err());
    }

    #[test]
    fn physical_write_does_not_trust_cached_stop_when_disconnected() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        drive.connected = false;
        drive.guard.read_only = false;
        drive.sim_regs.insert(0x2100, 4);
        assert!(drive.write_register_safe_ex(0x2001, 5000, None).is_err());
        assert_eq!(drive.tx_count, 0);
        assert!(drive.guard.read_only);
    }

    #[test]
    fn legacy_client_stop_hint_cannot_bypass_a_running_motor_interlock() {
        let mut drive = DriveSession::default();
        drive.guard.read_only = false;
        drive.simulated = true;
        drive.sim_regs.insert(0x2100, 1);
        assert!(drive.write_register_safe_ex(0x0101, 0, None).is_err());
        assert_eq!(drive.tx_count, 0);
    }

    #[test]
    fn eeprom_write_is_blocked_before_modbus_io() {
        let mut drive = DriveSession::default();
        drive.guard.read_only = false;
        assert!(drive.write_register_safe_ex(0x8101, 1, None).is_err());
        assert_eq!(drive.tx_count, 0);
    }

    #[test]
    fn connecting_or_disconnecting_revokes_previous_unlock() {
        let mut drive = DriveSession::default();
        drive.guard.read_only = false;
        drive.disconnect();
        assert!(drive.guard.read_only);
        drive.guard.read_only = false;
        drive.connect_or_configure(true, "COM1".into(), 9600, "8N1".into(), 1, 400).unwrap();
        assert!(drive.guard.read_only);
    }

    #[test]
    fn reverse_frequency_is_signed_but_modbus_raw_is_preserved() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        // 0xEC78 is -5000 in two's complement: -50.00 Hz.
        drive.accept_reading(0x3000, 1, &response(&[1, 3, 2, 0xEC, 0x78]))
            .unwrap();
        let snapshot = drive.telemetry_snapshot();
        assert_eq!(snapshot.output_freq_raw, Some(0xEC78));
        assert_eq!(snapshot.output_freq_hz, Some(-50.0));
        assert_eq!(snapshot.dc_bus_volt, None);
    }

    #[test]
    fn physical_snapshot_uses_only_validated_reads_from_this_connection() {
        let mut drive = DriveSession::default();
        assert_eq!(drive.telemetry_snapshot().dc_bus_volt, None);
        drive.simulated = true;
        assert_eq!(drive.telemetry_snapshot().dc_bus_volt, Some(538));
        drive.simulated = false;
        assert_eq!(drive.telemetry_snapshot().dc_bus_volt, None);
        drive.accept_reading(0x3002, 2, &response(&[1, 3, 4, 0, 0, 0, 125])).unwrap();
        let current = drive.telemetry_snapshot();
        assert_eq!(current.output_volt, Some(0));
        assert_eq!(current.output_current_raw, Some(125));
        assert_eq!(current.output_current_a, Some(12.5));
        assert_eq!(current.dc_bus_volt, None);
        drive.reset_measurements();
        assert_eq!(drive.telemetry_snapshot().output_current_a, None);
    }

    #[test]
    fn stale_or_rejected_readings_do_not_appear_as_current() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        drive.accept_reading(0x3002, 1, &response(&[1, 3, 2, 0, 10])).unwrap();
        drive.accept_reading(0x3006, 1, &response(&[1, 3, 2, 2, 20])).unwrap();
        assert_eq!(drive.telemetry_snapshot().dc_bus_volt, Some(532));
        assert!(drive.accept_reading(0x3006, 1, &response(&[1, 0x83, 2])).is_err());
        assert_eq!(drive.sample(0x3006), None);
        assert_eq!(drive.sample(0x3002), Some(10));
        drive.measured.get_mut(&0x3002).unwrap().1 = Instant::now() - Duration::from_secs(6);
        assert_eq!(drive.sample(0x3002), None);
    }

    #[test]
    fn physical_direction_and_fault_require_current_session_replies() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        drive.accept_reading(0x3000, 1, &response(&[1, 3, 2, 0xEC, 0x78])).unwrap();
        let first = drive.telemetry_snapshot();
        assert_eq!(first.output_freq_hz, Some(-50.0));
        assert_eq!((first.state_code, first.status_bits, first.fault_code), (None, None, None));
        for reg in [0x2100, 0x2101, 0x2102] {
            assert_eq!(TELEMETRY_SINGLE_REG_POLL_ORDER.iter().filter(|&&r| r == reg).count(), 1);
        }
        drive.accept_reading(0x2101, 1, &response(&[1, 3, 2, 0, 2])).unwrap();
        drive.accept_reading(0x2100, 1, &response(&[1, 3, 2, 0, 6])).unwrap();
        drive.accept_reading(0x2102, 1, &response(&[1, 3, 2, 0, 14])).unwrap();
        let alarm = drive.telemetry_snapshot();
        assert_eq!((alarm.status_bits, alarm.fault_code), (Some(2), Some(14)));
        drive.accept_reading(0x2100, 1, &response(&[1, 3, 2, 0, 4])).unwrap();
        assert_eq!(drive.telemetry_snapshot().fault_code, None);
        drive.accept_reading(0x2102, 1, &response(&[1, 3, 2, 0, 0])).unwrap();
        assert_eq!(drive.telemetry_snapshot().fault_code, Some(0));
        drive.reset_measurements();
        let next = drive.telemetry_snapshot();
        assert_eq!((next.state_code, next.status_bits, next.fault_code), (None, None, None));
        assert_eq!(next.warning_code, None);
    }

    #[test]
    fn missing_direction_preserves_frequency_and_expired_fault_is_unknown() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        drive.accept_reading(0x3000, 1, &response(&[1, 3, 2, 0, 100])).unwrap();
        drive.accept_reading(0x2101, 2, &response(&[1, 3, 4, 0, 0, 0, 14])).unwrap();
        assert_eq!(drive.telemetry_snapshot().status_bits, Some(0));
        assert!(drive.accept_reading(0x2101, 1, &response(&[1, 0x83, 2])).is_err());
        assert_eq!(drive.telemetry_snapshot().status_bits, None);
        assert_eq!(drive.telemetry_snapshot().output_freq_hz, Some(1.0));
        drive.measured.get_mut(&0x2102).unwrap().1 = Instant::now() - Duration::from_secs(6);
        assert_eq!(drive.telemetry_snapshot().fault_code, None);
    }

    #[test]
    fn invalid_physical_fc03_reply_revokes_write_permission() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        drive.accept_reading(0x3002, 1, &response(&[1, 3, 2, 0, 1])).unwrap();
        drive.guard.read_only = false;
        assert!(drive.accept_reading(0x3002, 1, &response(&[1, 4, 2, 0, 1])).is_err());
        assert!(drive.guard.read_only);
        assert!(!drive.connected);
        assert_eq!(drive.sample(0x3002), None);
    }

    #[test]
    fn malformed_reply_cannot_refresh_a_physical_sample() {
        let mut drive = DriveSession::default();
        drive.simulated = false;
        for payload in [[1, 4, 2, 0, 1], [1, 3, 1, 0, 1], [2, 3, 2, 0, 1]] {
            drive.accept_reading(0x3002, 1, &response(&[1, 3, 2, 0, 1])).unwrap();
            assert!(drive.accept_reading(0x3002, 1, &response(&payload)).is_err());
            assert_eq!(drive.sample(0x3002), None);
        }
    }
}
