//! Windows "Save As" dialog for the two formats exported by the configurator.
use std::{ffi::OsString, io::Write, os::windows::ffi::OsStringExt, path::PathBuf};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OPENFILENAMEW, OFN_EXPLORER, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
};

pub fn save(owner: isize, format: &str, content: &str) -> Result<Option<String>, String> {
    let (filename, filter) = match format {
        "ods" => ("ONI_A650M_Profile.ods", "Профиль ONI (*.ods)\0*.ods\0\0"),
        "csv" => ("parameters.csv", "Параметры (*.csv)\0*.csv\0\0"),
        _ => return Err("Неизвестный формат экспорта".into()),
    };
    let mut name = vec![0u16; 32768];
    for (to, from) in name.iter_mut().zip(filename.encode_utf16()) { *to = from; }
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let ext: Vec<u16> = format.encode_utf16().chain(Some(0)).collect();
    // All string buffers live through the synchronous call on this worker.
    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = owner as _;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = name.as_mut_ptr();
    dialog.nMaxFile = name.len() as u32;
    dialog.lpstrDefExt = ext.as_ptr();
    dialog.Flags = OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 { Ok(None) } else { Err(format!("Диалог сохранения: 0x{error:X}")) };
    }
    let length = name.iter().position(|&v| v == 0).ok_or("Некорректный путь")?;
    let path = PathBuf::from(OsString::from_wide(&name[..length]));
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().ok_or("Нужна папка назначения")?)
        .map_err(|e| e.to_string())?;
    temp.write_all(content.as_bytes()).and_then(|_| temp.as_file().sync_all()).map_err(|e| e.to_string())?;
    temp.persist(&path).map_err(|e| e.to_string())?;
    Ok(Some(path.display().to_string()))
}
