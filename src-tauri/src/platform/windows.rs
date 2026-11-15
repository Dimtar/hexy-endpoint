use super::Mounted;
use std::process::Command;
use windows_sys::Win32::NetworkManagement::WNet::{
    WNetAddConnection2W, WNetCancelConnection2W, WNetGetConnectionW, NETRESOURCEW,
    RESOURCETYPE_DISK,
};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The UNC path a drive letter is mapped to, if it is a network drive.
fn remote_of(letter: char) -> Option<String> {
    let local = wide(&format!("{letter}:"));
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let rc = unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

pub fn mounted(host: &str) -> Vec<Mounted> {
    let prefix = format!("\\\\{}\\", host.to_lowercase());
    ('A'..='Z')
        .filter_map(|letter| {
            let remote = remote_of(letter)?;
            let share = remote.to_lowercase().starts_with(&prefix).then(|| remote[prefix.len()..].to_string())?;
            Some(Mounted {
                share,
                location: format!("{letter}:"),
            })
        })
        .collect()
}

fn in_use(letter: char) -> bool {
    let used = unsafe { GetLogicalDrives() };
    used & (1 << (letter as u32 - 'A' as u32)) != 0 || remote_of(letter).is_some()
}

/// Highest free drive letter, skipping A-C and any letter set aside for
/// another share.
fn free_letter(reserved: &[char]) -> Option<char> {
    ('D'..='Z')
        .rev()
        .find(|l| !reserved.contains(l) && !in_use(*l))
}

/// `drive_letter` is the letter the user picked for this share, if any;
/// otherwise a free one is used, avoiding `reserved_letters`.
pub fn mount(
    host: &str,
    share: &str,
    user: &str,
    password: &str,
    drive_letter: Option<char>,
    reserved_letters: &[char],
) -> Result<(), String> {
    let letter = match drive_letter.map(|l| l.to_ascii_uppercase()) {
        Some(l) if !('D'..='Z').contains(&l) => {
            return Err(format!("{l}: cannot be used as a drive letter"))
        }
        Some(l) if in_use(l) => return Err(format!("Drive {l}: is already in use")),
        Some(l) => l,
        None => free_letter(reserved_letters).ok_or("no free drive letter")?,
    };
    let local = wide(&format!("{letter}:"));
    let remote = wide(&format!("\\\\{host}\\{share}"));
    let user = wide(user);
    let password = wide(password);
    let mut res: NETRESOURCEW = unsafe { std::mem::zeroed() };
    res.dwType = RESOURCETYPE_DISK;
    res.lpLocalName = local.as_ptr() as *mut u16;
    res.lpRemoteName = remote.as_ptr() as *mut u16;
    // Flags 0: not remembered by Windows. We remap at every login ourselves,
    // so a share removed on the server never leaves a dead drive behind.
    let rc = unsafe { WNetAddConnection2W(&res, password.as_ptr(), user.as_ptr(), 0) };
    match rc {
        0 => Ok(()),
        86 | 1326 => Err("the server rejected the username or password".into()),
        1219 => Err(
            "Windows is already connected to this server as a different user; disconnect that connection first".into(),
        ),
        53 | 67 => Err("the server or share could not be found".into()),
        other => Err(std::io::Error::from_raw_os_error(other as i32).to_string()),
    }
}

/// Whether the taskbar uses the dark theme (the Windows default).
pub fn taskbar_is_dark() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut light: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut light as *mut u32 as *mut _,
            &mut size,
        )
    };
    rc != 0 || light == 0
}

pub fn unmount(_host: &str, m: &Mounted) -> Result<(), String> {
    let local = wide(&m.location);
    let rc = unsafe { WNetCancelConnection2W(local.as_ptr(), 0, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(rc as i32).to_string())
    }
}

pub fn open(_host: &str, m: &Mounted) -> Result<(), String> {
    Command::new("explorer")
        .arg(format!("{}\\", m.location))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

