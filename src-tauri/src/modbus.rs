//! Pure Modbus RTU framing (`FC03` Read Holding Registers, `FC06` Write Single Register)
//! and CRC-16/MODBUS calculation for ONI A650 / A650M.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const FC_READ_REGISTERS: u8 = 0x03;
pub const FC_WRITE_SINGLE_REGISTER: u8 = 0x06;
pub const EXCEPTION_BIT: u8 = 0x80;
pub const MAX_FC03_REGISTERS: u16 = 8; // Per ONI_A650M_Manual.pdf Appendix A.5

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModbusError {
    #[error("slave address {0} is out of valid range 1..=247")]
    InvalidSlaveAddress(u8),
    #[error("FC03 register count {0} is out of range 1..=8 (A650 Appendix A.5 limit)")]
    InvalidReadCount(u16),
    #[error("frame too short: {0} bytes")]
    FrameTooShort(usize),
    #[error("CRC-16 mismatch: expected 0x{expected:04X}, got 0x{actual:04X}")]
    CrcMismatch { expected: u16, actual: u16 },
    #[error("slave address mismatch in response: expected {expected}, got {actual}")]
    SlaveMismatch { expected: u8, actual: u8 },
    #[error("Modbus exception 0x{code:02X} ({name}) on function 0x{function:02X}")]
    DriveException {
        function: u8,
        code: u8,
        name: &'static str,
    },
    #[error("unexpected response length: expected {expected}, got {actual}")]
    UnexpectedLength { expected: usize, actual: usize },
    #[error("write echo mismatch: sent (0x{sent_addr:04X}, 0x{sent_val:04X}), echoed (0x{echo_addr:04X}, 0x{echo_val:04X})")]
    WriteEchoMismatch {
        sent_addr: u16,
        sent_val: u16,
        echo_addr: u16,
        echo_val: u16,
    },
}

/// Computes standard CRC-16/MODBUS (polynomial `0xA001`, init `0xFFFF`, little-endian on wire).
pub fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            if (crc & 0x0001) != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

pub fn format_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn exception_name(code: u8) -> &'static str {
    match code {
        0x01 => "IllegalFunction",
        0x02 => "IllegalDataAddress",
        0x03 => "IllegalDataValue",
        0x04 => "OperationNotExecuted",
        0x11 => "ReadOnlyParameter",
        _ => "UnknownException",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRegistersRequest {
    pub slave: u8,
    pub address: u16,
    pub count: u16,
}

impl ReadRegistersRequest {
    pub fn new(slave: u8, address: u16, count: u16) -> Result<Self, ModbusError> {
        if !(1..=247).contains(&slave) {
            return Err(ModbusError::InvalidSlaveAddress(slave));
        }
        if !(1..=MAX_FC03_REGISTERS).contains(&count) {
            return Err(ModbusError::InvalidReadCount(count));
        }
        Ok(Self {
            slave,
            address,
            count,
        })
    }

    pub fn to_bytes(self) -> [u8; 8] {
        let mut buf = [
            self.slave,
            FC_READ_REGISTERS,
            (self.address >> 8) as u8,
            (self.address & 0xFF) as u8,
            (self.count >> 8) as u8,
            (self.count & 0xFF) as u8,
            0,
            0,
        ];
        let crc = crc16_modbus(&buf[..6]);
        buf[6] = (crc & 0xFF) as u8;
        buf[7] = (crc >> 8) as u8;
        buf
    }

    pub fn expected_response_len(self) -> usize {
        5 + (self.count as usize) * 2
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRegisterRequest {
    pub slave: u8,
    pub address: u16,
    pub value: u16,
}

impl WriteRegisterRequest {
    pub fn new(slave: u8, address: u16, value: u16) -> Result<Self, ModbusError> {
        if !(1..=247).contains(&slave) {
            return Err(ModbusError::InvalidSlaveAddress(slave));
        }
        Ok(Self {
            slave,
            address,
            value,
        })
    }

    pub fn to_bytes(self) -> [u8; 8] {
        let mut buf = [
            self.slave,
            FC_WRITE_SINGLE_REGISTER,
            (self.address >> 8) as u8,
            (self.address & 0xFF) as u8,
            (self.value >> 8) as u8,
            (self.value & 0xFF) as u8,
            0,
            0,
        ];
        let crc = crc16_modbus(&buf[..6]);
        buf[6] = (crc & 0xFF) as u8;
        buf[7] = (crc >> 8) as u8;
        buf
    }
}

pub fn verify_frame_crc(frame: &[u8]) -> Result<(), ModbusError> {
    if frame.len() < 5 {
        return Err(ModbusError::FrameTooShort(frame.len()));
    }
    let body = &frame[..frame.len() - 2];
    let expected = crc16_modbus(body);
    let actual = u16::from(frame[frame.len() - 2]) | (u16::from(frame[frame.len() - 1]) << 8);
    if expected != actual {
        return Err(ModbusError::CrcMismatch { expected, actual });
    }
    Ok(())
}

pub fn parse_read_response(expected_slave: u8, expected_count: u16, frame: &[u8]) -> Result<Vec<u16>, ModbusError> {
    verify_frame_crc(frame)?;
    if frame[0] != expected_slave {
        return Err(ModbusError::SlaveMismatch {
            expected: expected_slave,
            actual: frame[0],
        });
    }
    let fc = frame[1];
    if (fc & EXCEPTION_BIT) != 0 {
        let code = frame[2];
        return Err(ModbusError::DriveException {
            function: fc & !EXCEPTION_BIT,
            code,
            name: exception_name(code),
        });
    }
    let expected_len = 5 + (expected_count as usize) * 2;
    if frame.len() != expected_len {
        return Err(ModbusError::UnexpectedLength {
            expected: expected_len,
            actual: frame.len(),
        });
    }
    let mut values = Vec::with_capacity(expected_count as usize);
    for chunk in frame[3..frame.len() - 2].chunks_exact(2) {
        values.push((u16::from(chunk[0]) << 8) | u16::from(chunk[1]));
    }
    Ok(values)
}

pub fn parse_write_response(req: WriteRegisterRequest, frame: &[u8]) -> Result<(), ModbusError> {
    verify_frame_crc(frame)?;
    if frame[0] != req.slave {
        return Err(ModbusError::SlaveMismatch {
            expected: req.slave,
            actual: frame[0],
        });
    }
    let fc = frame[1];
    if (fc & EXCEPTION_BIT) != 0 {
        let code = frame[2];
        return Err(ModbusError::DriveException {
            function: fc & !EXCEPTION_BIT,
            code,
            name: exception_name(code),
        });
    }
    if frame.len() != 8 {
        return Err(ModbusError::UnexpectedLength {
            expected: 8,
            actual: frame.len(),
        });
    }
    let echo_addr = (u16::from(frame[2]) << 8) | u16::from(frame[3]);
    let echo_val = (u16::from(frame[4]) << 8) | u16::from(frame[5]);
    if echo_addr != req.address || echo_val != req.value {
        return Err(ModbusError::WriteEchoMismatch {
            sent_addr: req.address,
            sent_val: req.value,
            echo_addr,
            echo_val,
        });
    }
    Ok(())
}
