use super::Mounted;
use core_foundation::array::CFArrayRef;
use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFMutableDictionary, CFMutableDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::{CFURL, CFURLRef};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::process::Command;
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
        .filter_map(parse_mount_line)
        .filter(|(server, _)| same_server(server, host))
        .map(|(_, m)| m)
        .collect()
}

/// Whether two addresses are the same machine. macOS will not mount a share
/// twice from one server, so a share mounted via the server's IP has to count
/// as mounted when the app is configured with its name, and the reverse.
fn same_server(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    let (a, b) = (resolve(a), resolve(b));
    a.iter().any(|ip| b.contains(ip))
}

/// Resolves a host (optionally `host:port`) to its addresses, remembering
/// the answer for a while so the once-a-minute sync does not hammer DNS.
fn resolve(host: &str) -> Vec<IpAddr> {
    const KEEP: Duration = Duration::from_secs(600);
    static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<IpAddr>)>>> = Mutex::new(None);

    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.parse::<u16>().is_ok() => name,
        _ => host,
    };
    let key = name.to_lowercase();
    if let Some((at, ips)) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        if at.elapsed() < KEEP {
            return ips.clone();
        }
    }
    let ips: Vec<IpAddr> = match name.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => (name, 445)
            .to_socket_addrs()
            .map(|addrs| addrs.map(|a| a.ip()).collect())
            .unwrap_or_default(),
    };
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(key, (Instant::now(), ips.clone()));
    ips
}

/// Returns the server a mount came from, and the mount.
fn parse_mount_line(line: &str) -> Option<(String, Mounted)> {
    let rest = line.strip_prefix("//")?;
    let (source, rest) = rest.split_once(" on ")?;
    let (path, flags) = rest.rsplit_once(" (")?;
    if !flags.starts_with("smbfs") {
        return None;
    }
    let (server, share) = source.split_once('/')?;
    let server = server.rsplit_once('@').map_or(server, |(_, h)| h);
    Some((
        server.to_string(),
        Mounted {
            share: percent_decode_str(share).decode_utf8_lossy().into_owned(),
            location: path.to_string(),
        },
    ))
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
        let (server, m) = parse_mount_line(
            "//dan@192.168.1.10/My%20Media on /Volumes/My Media (smbfs, nodev, nosuid, mounted by dan)",
        )
        .unwrap();
        assert_eq!(server, "192.168.1.10");
        assert_eq!(m.share, "My Media");
        assert_eq!(m.location, "/Volumes/My Media");
    }

    #[test]
    fn ignores_other_filesystems() {
        assert!(parse_mount_line("/dev/disk3s1 on / (apfs, local)").is_none());
        assert!(parse_mount_line("//dan@nas/x on /Volumes/x (afpfs, nodev)").is_none());
    }

    #[test]
    fn matches_a_server_by_name_or_address() {
        assert!(same_server("NAS", "nas"));
        assert!(same_server("localhost", "127.0.0.1"));
        assert!(same_server("127.0.0.1:4455", "localhost"));
        assert!(!same_server("192.168.1.10", "192.168.1.11"));
        assert!(!same_server("no-such-host.invalid", "127.0.0.1"));
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
