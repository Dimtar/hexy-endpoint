//! Linux mounting, by one of two routes:
//!
//! - **Kernel (CIFS) mounts**, when the .deb or .rpm package is installed.
//!   Shares become ordinary folders under `/run/hexy-endpoint/<uid>/`, with
//!   `~/HexOS` pointing there, and work in every application. Mounting needs
//!   root, so it goes through the packaged helper via `pkexec`.
//! - **gvfs (`gio mount`)**, as a fallback for the AppImage. It needs no
//!   root but only works where the gvfs SMB backend is installed (GNOME-style
//!   desktops), and shares are only visible to some applications.

use super::Mounted;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const HELPER: &str = "/usr/lib/hexy-endpoint/hexy-endpoint-mount-helper";
const KERNEL_BASE: &str = "/run/hexy-endpoint";

fn uid() -> u32 {
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// Where this user's kernel mounts live.
fn kernel_dir() -> PathBuf {
    Path::new(KERNEL_BASE).join(uid().to_string())
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
    })
}

/// Whether the packaged helper is installed, so kernel mounts can be used.
fn kernel_available() -> bool {
    Path::new(HELPER).is_file() && on_path("pkexec")
}

fn uri(host: &str, share: &str) -> String {
    format!(
        "smb://{host}/{}",
        utf8_percent_encode(share, NON_ALPHANUMERIC)
    )
}

const NO_BACKEND: &str = "This desktop cannot mount shares for the AppImage. \
    Install Hexy Endpoint from the .deb or .rpm package instead, or install the \
    gvfs SMB backend (gvfs-smb or gvfs-backends).";

fn gio_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        NO_BACKEND.into()
    } else {
        e.to_string()
    }
}

/// Undoes the octal escapes used in /proc/self/mounts (`\040` is a space).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let value = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 8).ok());
            if let Some(value) = value {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses a /proc/self/mounts line such as
/// `//nas/My\040Docs /run/hexy-endpoint/1000/My\040Docs cifs rw,... 0 0`.
fn parse_kernel_line(line: &str, host: &str) -> Option<Mounted> {
    let mut fields = line.split(' ');
    let source = unescape(fields.next()?);
    let target = unescape(fields.next()?);
    let fstype = fields.next()?;
    if fstype != "cifs" && fstype != "smb3" {
        return None;
    }
    let (server, share) = source.strip_prefix("//")?.split_once('/')?;
    if !server.eq_ignore_ascii_case(host) || share.is_empty() {
        return None;
    }
    Some(Mounted {
        share: share.trim_end_matches('/').to_string(),
        location: target,
    })
}

fn kernel_mounted(host: &str) -> Vec<Mounted> {
    std::fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| parse_kernel_line(line, host))
        .collect()
}

/// `gio mount -l` prints lines like `Mount(0): media on nas -> smb://nas/media/`.
fn parse_gio_line(line: &str, host: &str) -> Option<Mounted> {
    let (_, target) = line.split_once("-> smb://")?;
    let (server, share) = target.trim().trim_end_matches('/').split_once('/')?;
    let server = server.rsplit_once('@').map_or(server, |(_, h)| h);
    if !server.eq_ignore_ascii_case(host) || share.is_empty() {
        return None;
    }
    let share = percent_decode_str(share).decode_utf8_lossy().into_owned();
    Some(Mounted {
        location: uri(host, &share),
        share,
    })
}

fn gio_mounted(host: &str) -> Vec<Mounted> {
    let Ok(out) = Command::new("gio").args(["mount", "-l"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| parse_gio_line(line, host))
        .collect()
}

pub fn mounted(host: &str) -> Vec<Mounted> {
    let mut all = kernel_mounted(host);
    for m in gio_mounted(host) {
        if !all.iter().any(|k| k.share.eq_ignore_ascii_case(&m.share)) {
            all.push(m);
        }
    }
    all
}

/// Turns what `pkexec` and `mount.cifs` print into something readable.
fn describe_helper_error(stderr: &str) -> String {
    let text = stderr.trim();
    if text.contains("error(13)") {
        "the server rejected the username or password, or this account has no access to the share".into()
    } else if text.contains("error(2)") || text.contains("error(6)") {
        "the share no longer exists on the server".into()
    } else if ["error(101)", "error(110)", "error(111)", "error(112)", "error(113)", "error(115)"]
        .iter()
        .any(|code| text.contains(code))
    {
        "the server could not be reached".into()
    } else if text.contains("Not authorized") || text.contains("Error executing command as another user") {
        "this session is not allowed to mount shares; log in at the computer itself, or ask an administrator".into()
    } else if text.is_empty() {
        "mounting failed".into()
    } else {
        // Keep the first line: mount.cifs follows its error with a hint.
        text.lines().next().unwrap_or(text).to_string()
    }
}

fn run_helper(args: &[&str], stdin: Option<&str>) -> Result<(), String> {
    let mut child = Command::new("pkexec")
        .arg(HELPER)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run pkexec: {e}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        if let Some(text) = stdin {
            let _ = pipe.write_all(text.as_bytes());
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(describe_helper_error(&String::from_utf8_lossy(&out.stderr)))
    }
}

/// Makes `~/HexOS` point at the mounts, unless something is already there.
fn link_home_folder() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let link = Path::new(&home).join("HexOS");
    if std::fs::symlink_metadata(&link).is_err() {
        let _ = std::os::unix::fs::symlink(kernel_dir(), link);
    }
}

fn gio_mount(host: &str, share: &str, user: &str, password: &str) -> Result<(), String> {
    let mut child = Command::new("gio")
        .args(["mount", &uri(host, share)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(gio_error)?;
    // gio asks for user, domain and password in that order; an empty domain
    // line accepts its default.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(stdin, "{user}\n\n{password}\n");
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if msg.contains("already mounted") {
        Ok(())
    } else if msg.contains("doesn\u{2019}t implement mount")
        || msg.contains("doesn't implement mount")
        || msg.contains("not supported")
    {
        Err(NO_BACKEND.into())
    } else {
        Err(msg)
    }
}

pub fn mount(
    host: &str,
    share: &str,
    user: &str,
    password: &str,
    _drive_letter: Option<char>,
    _reserved_letters: &[char],
) -> Result<(), String> {
    if kernel_available() {
        run_helper(&["mount", host, share, user], Some(password))?;
        link_home_folder();
        Ok(())
    } else {
        gio_mount(host, share, user, password)
    }
}

/// Most Linux panels are dark; there is no portable way to ask.
pub fn taskbar_is_dark() -> bool {
    true
}

pub fn unmount(_host: &str, m: &Mounted) -> Result<(), String> {
    if m.location.starts_with("smb://") {
        let out = Command::new("gio")
            .args(["mount", "-u", &m.location])
            .output()
            .map_err(gio_error)?;
        return if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        };
    }
    // Only folders the helper made are ours to unmount.
    let path = Path::new(&m.location);
    match (path.parent(), path.file_name().and_then(|n| n.to_str())) {
        (Some(parent), Some(name)) if parent == kernel_dir() => run_helper(&["unmount", name], None),
        _ => Err(format!(
            "{} was not mounted by Hexy Endpoint; unmount it yourself",
            m.location
        )),
    }
}

pub fn open(_host: &str, m: &Mounted) -> Result<(), String> {
    let (program, missing) = if m.location.starts_with("smb://") {
        ("gio", NO_BACKEND.to_string())
    } else {
        ("xdg-open", "xdg-open was not found".to_string())
    };
    let mut command = Command::new(program);
    if program == "gio" {
        command.arg("open");
    }
    command
        .arg(&m.location)
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing
            } else {
                e.to_string()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gio_mount_lines() {
        let m = parse_gio_line("Mount(0): My Media on nas -> smb://nas/My%20Media/", "NAS").unwrap();
        assert_eq!(m.share, "My Media");
        assert_eq!(m.location, "smb://NAS/My%20Media");
        assert!(parse_gio_line("Mount(1): x on other -> smb://other/x/", "nas").is_none());
        assert!(parse_gio_line("Volume(0): disk", "nas").is_none());
    }

    #[test]
    fn parses_kernel_mount_lines() {
        let m = parse_kernel_line(
            "//fable.local/My\\040Docs /run/hexy-endpoint/1000/My\\040Docs cifs rw,nosuid,nodev,relatime,vers=3.1.1 0 0",
            "Fable.local",
        )
        .unwrap();
        assert_eq!(m.share, "My Docs");
        assert_eq!(m.location, "/run/hexy-endpoint/1000/My Docs");
        assert!(parse_kernel_line("//other/x /mnt/x cifs rw 0 0", "fable.local").is_none());
        assert!(parse_kernel_line("/dev/sda1 / ext4 rw 0 0", "fable.local").is_none());
        assert!(parse_kernel_line("//fable.local/x /mnt/x nfs rw 0 0", "fable.local").is_none());
    }

    #[test]
    fn explains_mount_failures() {
        assert!(describe_helper_error("mount error(13): Permission denied\nRefer to the mount.cifs(8) manual page")
            .contains("rejected"));
        assert!(describe_helper_error("mount error(113): could not connect to 10.0.0.9").contains("reached"));
        assert_eq!(
            describe_helper_error("mount.cifs was not found; install the cifs-utils package"),
            "mount.cifs was not found; install the cifs-utils package"
        );
    }
}
