#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(feature = "gui"))]
use a650_core::actor::DriveSession;

#[cfg(feature = "gui")]
mod gui_app {
    use a650_core::actor::{
        enumerate_ports, ConnectionStatusDto, DriveSession, DriveTelemetryDto,
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
    ) -> Result<ConnectionStatusDto, String> {
        let sess = state.0.lock().map_err(|e| e.to_string())?;
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
    pub fn poll_drive_telemetry(
        state: tauri::State<'_, SharedDriveState>,
    ) -> Result<DriveTelemetryDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.poll_telemetry()
    }

    #[tauri::command]
    pub fn set_safety_allow_write(
        state: tauri::State<'_, SharedDriveState>,
        allow_write: bool,
    ) -> Result<bool, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.guard.read_only = !allow_write;
        sess.record_audit(
            "set_safety_guard",
            serde_json::json!({ "allow_write": allow_write }),
        );
        Ok(allow_write)
    }

    #[tauri::command]
    pub fn write_register_fc06(
        state: tauri::State<'_, SharedDriveState>,
        address: u16,
        value: u16,
        stop_only_code: Option<String>,
    ) -> Result<RegisterExchangeResultDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.write_register_safe_ex(address, value, stop_only_code.as_deref())
    }

    #[tauri::command]
    pub fn read_registers_fc03(
        state: tauri::State<'_, SharedDriveState>,
        address: u16,
        count: u16,
    ) -> Result<RegisterExchangeResultDto, String> {
        let mut sess = state.0.lock().map_err(|e| e.to_string())?;
        sess.read_registers_ex(address, count)
    }

    #[tauri::command]
    pub fn calc_parameter_addresses(code: String) -> Result<(u16, Option<u16>), String> {
        let addrs = parameter_addresses(&code).map_err(|e| e.to_string())?;
        Ok((addrs.ram, addrs.eeprom))
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
                disconnect_serial,
                auto_scan_drive,
                poll_drive_telemetry,
                set_safety_allow_write,
                write_register_fc06,
                read_registers_fc03,
                calc_parameter_addresses,
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
