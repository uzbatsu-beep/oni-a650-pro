//! Thread-safe Drive Controller & Persistent RS-485 / Simulator Actor for ONI A650 / A650M.

use crate::addresses::parameter_addresses;
use crate::catalog::{load_embedded_faults, load_embedded_parameters, FaultRecord, ParameterRecord};
use crate::modbus::{
    ModbusError,
    crc16_modbus, format_hex, parse_read_response, parse_write_response, ReadRegistersRequest,
    WriteRegisterRequest,
};
use crate::safety::{AuditLogEntry, SafetyError, SafetyGuard};
use serde::{Deserialize, Serialize};
use serialport::{ClearBuffer, DataBits, Parity, SerialPort, SerialPortType, StopBits};
use std::collections::HashMap;
use std::io::{Read, Write};
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
    pub session_id: u64,
    pub simulated: bool,
    pub connected: bool,
    pub write_enabled: bool,
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

pub struct DriveSession {
    session_id: u64,
    pub model: String,
    pub slave_id: u8,
    pub simulated: bool,
    pub connected: bool,
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
    pub last_bus_io: Option<Instant>,
    pub serial_handle: Option<Box<dyn SerialPort>>,
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
            model: "A650M".to_string(),
            slave_id: 1,
            simulated: true,
            connected: false,
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
            last_bus_io: None,
            serial_handle: None,
            measured: HashMap::new(),
            guard,
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
fn read_bytes_with_deadline(
    port: &mut dyn SerialPort,
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
            Ok(0) => {
                thread::sleep(Duration::from_millis(2));
            }
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
fn read_modbus_rtu_response(
    port: &mut dyn SerialPort,
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
        // Connection changes always revoke the operator's write unlock.
        self.guard.read_only = true;
        self.measured.clear();
        self.last_tx_hex.clear();
        self.last_rx_hex.clear();
        self.last_rtt_ms = 0;
        self.connected = false;
        self.poll_step_idx = 0;
    }

    fn sample(&self, address: u16) -> Option<u16> {
        if self.simulated {
            return self.sim_regs.get(&address).copied();
        }
        // One nominal eight-register sweep takes 2.8 s at a 350 ms tick.
        let (value, received) = self.measured.get(&address)?;
        (self.connected && received.elapsed() <= Duration::from_secs(5)).then_some(*value)
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
        if self.serial_handle.is_some() {
            return Ok(());
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
        self.serial_handle = Some(port);
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
        self.serial_handle = None;
        self.reset_measurements();
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

        // Open persistent handle & verify handshake on U00.00 (0x3000, count = 1)
        self.ensure_port_open()?;
        match self.read_registers_ex(0x3000, 1) {
            Ok(res) => {
                self.connected = true;
                self.last_error = None;
                self.record_audit(
                    "connect_rs485_ok",
                    serde_json::json!({
                        "port": self.port_name,
                        "baud": self.baud_rate,
                        "format": self.frame_format,
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
                self.serial_handle = None;
                self.last_error = Some(err.clone());
                self.record_audit(
                    "connect_rs485_failed",
                    serde_json::json!({
                        "port": self.port_name,
                        "baud": self.baud_rate,
                        "format": self.frame_format,
                        "slave_id": self.slave_id,
                        "error": err
                    }),
                );
                Err(err)
            }
        }
    }

    pub fn disconnect(&mut self) -> ConnectionStatusDto {
        self.serial_handle = None;
        self.reset_measurements();
        self.simulated = true;
        self.last_error = None;
        self.record_audit("disconnect_rs485", serde_json::json!({ "simulated": true }));
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

    fn reject_write(&mut self, address: u16, value: u16, reason: String) -> Result<RegisterExchangeResultDto, String> {
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
        // Reject forbidden writes before any physical bus I/O.
        if let Err(err) = self.guard.check_address(self.slave_id, address) {
            return self.reject_write(address, value, err.to_string());
        }

        // FC03 state query MUST occur immediately before FC06 within the same
        // locked DriveSession. Never use simulator cache for physical writes.
        let fresh_state = if self.simulated {
            self.sim_regs.get(&0x2100).copied()
        } else {
            if !self.connected {
                self.guard.read_only = true;
                return self.reject_write(address, value,
                    "FC06 denied: physical connection not verified; SafetyGuard re-locked".to_string());
            }
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

        let parameter = self.parameters.iter().find(|p| {
            p.access == "RW" && parameter_addresses(&p.param_id).ok().map(|a| a.ram) == Some(address)
        });
        if let Err(err) = self.guard.check_write(self.slave_id, address, value, fresh_state, parameter) {
            return self.reject_write(address, value, err.to_string());
        }

        let req = WriteRegisterRequest::new(self.slave_id, address, value)
            .map_err(|e| e.to_string())?;
        let tx_bytes = req.to_bytes();
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

            let io_res: Result<Vec<u8>, String> = (|| {
                let port = self.serial_handle.as_mut().unwrap();
                let _ = port.clear(ClearBuffer::Input);
                port.write_all(&tx_bytes)
                    .map_err(|e| format!("Ошибка записи в {}: {}", self.port_name, e))?;
                let _ = port.flush();
                // Normal FC06 echo has 8 bytes (3 header + 5 tail)
                read_modbus_rtu_response(port.as_mut(), 5, timeout)
            })();

            self.last_bus_io = Some(Instant::now());

            let rx_buf = match io_res {
                Ok(buf) => buf,
                Err(_) => {
                    self.err_count += 1;
                    self.serial_handle = None;
                    self.connected = false;
                    self.measured.clear();
                    self.guard.read_only = true;
                    let msg = SafetyError::WriteOutcomeUnknown { address, value }.to_string();
                    self.last_error = Some(msg.clone());
                    return self.reject_write(address, value, msg);
                }
            };

            let rx_s = format_hex(&rx_buf);
            self.last_rx_hex = rx_s.clone();
            if let Err(err) = parse_write_response(req, &rx_buf) {
                self.err_count += 1;
                self.connected = false;
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
        let tx_bytes = req.to_bytes();
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
                self.measured.clear();
                self.guard.read_only = true;
                self.last_error = Some(e.clone());
                return Err(e);
            }
            self.enforce_interframe_guard();
            let timeout = Duration::from_millis(self.timeout_ms.max(200));
            let expected_tail = (count as usize) * 2 + 2;

            let io_res: Result<Vec<u8>, String> = (|| {
                let port = self.serial_handle.as_mut().unwrap();
                let _ = port.clear(ClearBuffer::Input);
                port.write_all(&tx_bytes)
                    .map_err(|e| format!("Ошибка отправки в {}: {}", self.port_name, e))?;
                let _ = port.flush();
                read_modbus_rtu_response(port.as_mut(), expected_tail, timeout)
            })();

            self.last_bus_io = Some(Instant::now());

            let rx_buf = match io_res {
                Ok(buf) => buf,
                Err(e) => {
                    self.err_count += 1;
                    self.connected = false;
                    self.measured.clear();
                    self.guard.read_only = true;
                    // Re-open on next attempt if OS serial handle glitched
                    self.serial_handle = None;
                    let msg = format!(
                        "Нет ответа ПЧ (ID={}, рег. 0x{:04X}, {} шт.) на {}: {}",
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
                let msg = format!("Рег. 0x{:04X}: {} (RX: {})", address, e, rx_hex);
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
        let values = parse_read_response(self.slave_id, count, frame).inspect_err(|err| {
            if matches!(err, ModbusError::DriveException { .. }) {
                self.measured.retain(|addr, _| !addr.checked_sub(address).is_some_and(|n| n < count));
            } else {
                self.connected = false;
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
        if !self.simulated {
            let reg_addr =
                TELEMETRY_SINGLE_REG_POLL_ORDER[self.poll_step_idx % TELEMETRY_SINGLE_REG_POLL_ORDER.len()];
            self.poll_step_idx = (self.poll_step_idx + 1) % TELEMETRY_SINGLE_REG_POLL_ORDER.len();

            // A rejected register is unavailable in this snapshot. The next tick
            // continues the sweep even on firmware that rejects this address.
            let _ = self.read_registers_ex(reg_addr, 1);
        }

        Ok(self.telemetry_snapshot())
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
        let bauds = [9600u32, 19200, 38400, 4800, 57600, 115200];
        let formats = ["1-8-N-2", "1-8-N-1", "1-8-E-1", "1-8-O-1"];
        self.serial_handle = None;
        self.port_name = port_name.trim().to_string();
        self.slave_id = slave_id;
        self.simulated = false;
        self.timeout_ms = 220;

        for &b in &bauds {
            for &fmt in &formats {
                self.serial_handle = None;
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
        self.serial_handle = None;
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
