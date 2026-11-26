#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(all(feature = "gui", windows))]
mod file_dialog;

#[cfg(not(feature = "gui"))]
use a650_core::actor::DriveSession;

#[cfg(feature = "gui")]
mod gui_app {
    use a650_core::actor::{
        enumerate_ports, ConnectionStatusDto, EepromItem, EepromPreview, DriveSession, DriveTelemetryDto, EthernetProtocol, MonitorReadMode,
        RegisterExchangeResultDto, SerialPortInfoDto, SharedDriveState,
    };
    use a650_core::addresses::parameter_addresses;
    use a650_core::ods::{parse_studio_profile, serialize_studio_profile, StudioProfile};
    use std::sync::Mutex;

    #[tauri::command]
    pub fn open_project_repository() -> Result<(), String> {
        #[cfg(target_os = "windows")]
        let opener = "explorer.exe";
        #[cfg(target_os = "macos")]
        let opener = "open";
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let opener = "xdg-open";
        std::process::Command::new(opener)
            .arg(env!("CARGO_PKG_REPOSITORY"))
            .spawn()
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    #[tauri::command]
    pub fn list_serial_ports() -> Vec<SerialPortInfoDto> {
        enumerate_ports()
    }

    #[tauri::command]
    pub fn get_connection_status(
        state: tauri::State<'_, SharedDriveState>,
        session_id: u64,
    ) -> Result<ConnectionStatusDto, String> {
        let sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        Ok(sess.status_dto())
    }

    #[tauri::command]
    pub fn connect_serial(
        state: tauri::State<'_, SharedDriveState>,
        simulated: bool,
        port_name: String,
        baud_rate: u32,
        frame_format: String,
        slave_id: u8,
        timeout_ms: Option<u64>,
    ) -> Result<ConnectionStatusDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.connect_or_configure(
            simulated,
            port_name,
            baud_rate,
            frame_format,
            slave_id,
            timeout_ms.unwrap_or(400),
        )
    }

    #[tauri::command]
    pub async fn connect_ethernet(
        state: tauri::State<'_, SharedDriveState>, ip: String, port: u16, slave_id: u8, timeout_ms: Option<u64>,
        protocol: Option<EthernetProtocol>,
    ) -> Result<ConnectionStatusDto, String> {
        state.0.lock().map_err(|e| e.to_string())?
            .connect_ethernet_with_protocol(&ip, port, slave_id, timeout_ms.unwrap_or(500), protocol.unwrap_or_default())
    }

    #[tauri::command]
    pub fn disconnect_serial(
        state: tauri::State<'_, SharedDriveState>,
    ) -> Result<ConnectionStatusDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        Ok(sess.disconnect())
    }

    #[tauri::command]
    pub fn auto_scan_drive(
        state: tauri::State<'_, SharedDriveState>,
        port_name: String,
        slave_id: u8,
    ) -> Result<ConnectionStatusDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.auto_scan_port(port_name, slave_id)
    }

    #[tauri::command]
    pub async fn poll_drive_telemetry(
        state: tauri::State<'_, SharedDriveState>,
        session_id: u64,
        grouped: Option<bool>,
        read_mode: Option<MonitorReadMode>,
        period_ms: Option<u64>,
    ) -> Result<DriveTelemetryDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        let mode = read_mode.unwrap_or_else(|| {
            if grouped == Some(true) { MonitorReadMode::Block7 } else { MonitorReadMode::Single }
        });
        sess.poll_monitor_mode(mode, period_ms.unwrap_or(350))
    }

    #[tauri::command]
    pub fn set_safety_allow_write(
        state: tauri::State<'_, SharedDriveState>,
        session_id: u64,
        allow_write: bool,
    ) -> Result<bool, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        // Operator unlock is only possible on an offline simulator or a verified
        // live connection; reconnect/disconnect revokes any prior permission.
        if allow_write && !sess.simulated && !sess.connected {
            sess.guard.read_only = true;
            sess.record_audit("set_safety_guard_denied",
                serde_json::json!({ "reason": "physical drive not connected" }));
            return Err("ПЧ не подключён: SafetyGuard остаётся в режиме «Только чтение»".to_string());
        }
        sess.guard.read_only = !allow_write;
        let simulated = sess.simulated;
        sess.record_audit(
            "set_safety_guard",
            serde_json::json!({ "allow_write": allow_write, "simulated": simulated }),
        );
        Ok(!sess.guard.read_only)
    }

    #[tauri::command]
    pub fn write_register_fc06(
        state: tauri::State<'_, SharedDriveState>,
        session_id: u64,
        address: u16,
        value: u16,
        stop_only_code: Option<String>,
    ) -> Result<RegisterExchangeResultDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        sess.write_register_safe_ex(address, value, stop_only_code.as_deref())
    }

    #[tauri::command]
    pub fn eeprom_preview(state: tauri::State<'_, SharedDriveState>, session_id: u64) -> Result<EepromPreview, String> {
        let sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        Ok(sess.eeprom_preview())
    }

    #[tauri::command]
    pub async fn stage_eeprom_parameter(state: tauri::State<'_, SharedDriveState>, session_id: u64, code: String) -> Result<EepromItem, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        sess.stage_eeprom_parameter(&code)
    }

    #[tauri::command]
    pub async fn persist_eeprom_parameter(state: tauri::State<'_, SharedDriveState>, session_id: u64, code: String, expected_raw: u16) -> Result<RegisterExchangeResultDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        sess.persist_eeprom_parameter(&code, expected_raw)
    }

    #[tauri::command]
    pub fn read_registers_fc03(
        state: tauri::State<'_, SharedDriveState>,
        session_id: u64,
        address: u16,
        count: u16,
    ) -> Result<RegisterExchangeResultDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.require_session(session_id)?;
        sess.read_registers_ex(address, count)
    }

    #[tauri::command]
    pub fn calc_parameter_addresses(code: String) -> Result<(u16, Option<u16>), String> {
        let addrs = parameter_addresses(&code).map_err(|e| e.to_string())?;
        Ok((addrs.ram, addrs.eeprom))
    }

    #[tauri::command]
    pub async fn save_export_file(window: tauri::Window, format: String, content: String) -> Result<Option<String>, String> {
        #[cfg(windows)]
        {
            let owner = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
            tauri::async_runtime::spawn_blocking(move || crate::file_dialog::save(owner, &format, &content))
                .await.map_err(|e| e.to_string())?
        }
        #[cfg(not(windows))]
        {
            let _ = (window, format, content);
            Err("Системный диалог доступен в сборке для Windows.".into())
        }
    }

    #[tauri::command]
    pub fn import_ods_text(content: String) -> Result<StudioProfile, String> {
        parse_studio_profile(&content, Some("ПЧ ONI A650M")).map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub fn export_ods_text(profile: StudioProfile) -> String {
        serialize_studio_profile(&profile)
    }

    pub fn run() {
        tauri::Builder::default()
            .manage(SharedDriveState(Mutex::new(DriveSession::default())))
            .invoke_handler(tauri::generate_handler![
                open_project_repository,
                list_serial_ports,
                get_connection_status,
                connect_serial,
                connect_ethernet,
                disconnect_serial,
                auto_scan_drive,
                poll_drive_telemetry,
                set_safety_allow_write,
                write_register_fc06,
                eeprom_preview,
                stage_eeprom_parameter,
                persist_eeprom_parameter,
                read_registers_fc03,
                calc_parameter_addresses,
                save_export_file,
                import_ods_text,
                export_ods_text,
            ])
            .run(tauri::generate_context!())
            .expect("error while running ONI A650 PRO Tauri application");
    }
}

#[cfg(feature = "gui")]
fn main() {
    gui_app::run();
}

#[cfg(not(feature = "gui"))]
fn main() {
    let sess = DriveSession::default();
    let ports = a650_core::actor::enumerate_ports();
    println!(
        "ONI A650 PRO Rust Core initialized: {} parameters, {} fault codes loaded.",
        sess.parameters.len(),
        sess.faults.len()
    );
    if ports.is_empty() {
        println!("Available COM ports: none detected.");
    } else {
        println!("Available COM ports:");
        for p in ports {
            println!("  - {} ({})", p.name, p.description);
        }
    }
}
