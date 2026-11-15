use super::Mounted;
use std::process::Command;
use windows_sys::Win32::NetworkManagement::WNet::{
    WNetAddConnection2W, WNetCancelConnection2W, WNetGetConnectionW, NETRESOURCEW,
    RESOURCETYPE_DISK,
};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

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

/// Highest free drive letter, skipping A-C.
fn free_letter() -> Option<char> {
    let used = unsafe { GetLogicalDrives() };
    ('D'..='Z')
        .rev()
        .find(|&l| used & (1 << (l as u32 - 'A' as u32)) == 0 && remote_of(l).is_none())
}

pub fn mount(host: &str, share: &str, user: &str, password: &str) -> Result<(), String> {
    let letter = free_letter().ok_or("no free drive letter")?;
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

