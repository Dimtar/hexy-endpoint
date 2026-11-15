use super::Mounted;
use core_foundation::array::CFArrayRef;
use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFMutableDictionary, CFMutableDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::{CFURL, CFURLRef};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::process::Command;
use std::ptr;

#[link(name = "NetFS", kind = "framework")]
extern "C" {
    fn NetFSMountURLSync(
        url: CFURLRef,
        mountpath: CFURLRef,
        user: CFStringRef,
        passwd: CFStringRef,
        open_options: CFMutableDictionaryRef,
        mount_options: CFMutableDictionaryRef,
        mountpoints: *mut CFArrayRef,
    ) -> i32;
}

/// Parses `mount` output lines such as
/// `//user@host/share on /Volumes/share (smbfs, nodev, nosuid, mounted by me)`.
pub fn mounted(host: &str) -> Vec<Mounted> {
    let Ok(out) = Command::new("/sbin/mount").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| parse_mount_line(line, host))
        .collect()
}

fn parse_mount_line(line: &str, host: &str) -> Option<Mounted> {
    let rest = line.strip_prefix("//")?;
    let (source, rest) = rest.split_once(" on ")?;
    let (path, flags) = rest.rsplit_once(" (")?;
    if !flags.starts_with("smbfs") {
        return None;
    }
    let (server, share) = source.split_once('/')?;
    let server = server.rsplit_once('@').map_or(server, |(_, h)| h);
    if !server.eq_ignore_ascii_case(host) {
        return None;
    }
    Some(Mounted {
        share: percent_decode_str(share).decode_utf8_lossy().into_owned(),
        location: path.to_string(),
    })
}

pub fn mount(host: &str, share: &str, user: &str, password: &str) -> Result<(), String> {
    let url = format!(
        "smb://{host}/{}",
        utf8_percent_encode(share, NON_ALPHANUMERIC)
    );
    let cf_url_str = CFString::new(&url);
    let cf_user = CFString::new(user);
    let cf_pass = CFString::new(password);
    // Never put up a system login dialog: this runs unattended every minute.
    let mut open_options: CFMutableDictionary<CFString, CFString> = CFMutableDictionary::new();
    open_options.set(CFString::new("UIOption"), CFString::new("NoUI"));

    let code = unsafe {
        let raw = core_foundation_sys::url::CFURLCreateWithString(
            ptr::null(),
            cf_url_str.as_concrete_TypeRef(),
            ptr::null(),
        );
        if raw.is_null() {
            return Err(format!("invalid share address {url}"));
        }
        let cf_url = CFURL::wrap_under_create_rule(raw);
        let mut mountpoints: CFArrayRef = ptr::null();
        let code = NetFSMountURLSync(
            cf_url.as_concrete_TypeRef(),
            ptr::null(),
            cf_user.as_concrete_TypeRef(),
            cf_pass.as_concrete_TypeRef(),
            open_options.as_concrete_TypeRef(),
            ptr::null_mut(),
            &mut mountpoints,
        );
        if !mountpoints.is_null() {
            core_foundation_sys::base::CFRelease(mountpoints as *const _);
        }
        code
    };
    match code {
        0 | 17 => Ok(()), // 17 = EEXIST: already mounted
        13 | 80 => Err("the server rejected the username or password".into()),
        2 => Err("the share no longer exists on the server".into()),
        60 | 64 | 65 => Err("the server could not be reached".into()),
        other => Err(format!("mount failed (error {other})")),
    }
}

pub fn unmount(_host: &str, m: &Mounted) -> Result<(), String> {
    let out = Command::new("/usr/sbin/diskutil")
        .args(["unmount", &m.location])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if msg.is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        msg
    })
}

pub fn open(_host: &str, m: &Mounted) -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg(&m.location)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_smb_mount_lines() {
        let m = parse_mount_line(
            "//dan@192.168.1.10/My%20Media on /Volumes/My Media (smbfs, nodev, nosuid, mounted by dan)",
            "192.168.1.10",
        )
        .unwrap();
        assert_eq!(m.share, "My Media");
        assert_eq!(m.location, "/Volumes/My Media");
    }

    #[test]
    fn ignores_other_hosts_and_filesystems() {
        assert!(parse_mount_line("//dan@other/x on /Volumes/x (smbfs, nodev)", "nas").is_none());
        assert!(parse_mount_line("/dev/disk3s1 on / (apfs, local)", "nas").is_none());
        assert!(parse_mount_line("//dan@nas/x on /Volumes/x (afpfs, nodev)", "nas").is_none());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Mounts and unmounts a share on a real server. Run with
    /// `HEXY_TEST_HOST=... HEXY_TEST_USER=... HEXY_TEST_PASS=... HEXY_TEST_SHARE=... cargo test -- --ignored`.
    /// `HEXY_TEST_HOST` may carry a port, e.g. `127.0.0.1:4455`.
    #[test]
    #[ignore]
    fn mounts_and_unmounts_a_real_share() {
        let host = std::env::var("HEXY_TEST_HOST").expect("HEXY_TEST_HOST");
        let user = std::env::var("HEXY_TEST_USER").expect("HEXY_TEST_USER");
        let pass = std::env::var("HEXY_TEST_PASS").expect("HEXY_TEST_PASS");
        let share = std::env::var("HEXY_TEST_SHARE").expect("HEXY_TEST_SHARE");

        assert!(mount(&host, &share, &user, "definitely-wrong").is_err());
        mount(&host, &share, &user, &pass).expect("mount failed");
        let found = mounted(&host)
            .into_iter()
            .find(|m| m.share == share)
            .expect("share not in mount table");
        println!("mounted at {}", found.location);
        assert!(std::path::Path::new(&found.location).is_dir());
        // Mounting again is a no-op, not an error.
        mount(&host, &share, &user, &pass).expect("second mount failed");
        unmount(&host, &found).expect("unmount failed");
        assert!(mounted(&host).into_iter().all(|m| m.share != share));
    }
}
