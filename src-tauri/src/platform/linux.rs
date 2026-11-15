//! Linux mounts go through gvfs (`gio mount`), which needs no root and shows
//! the share in the file manager. Requires the gvfs SMB backend to be installed.

use super::Mounted;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::io::Write;
use std::process::{Command, Stdio};

fn uri(host: &str, share: &str) -> String {
    format!(
        "smb://{host}/{}",
        utf8_percent_encode(share, NON_ALPHANUMERIC)
    )
}

fn gio_missing(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "`gio` was not found; install gvfs and its SMB backend (gvfs-smb / gvfs-backends)".into()
    } else {
        e.to_string()
    }
}

/// `gio mount -l` prints lines like `Mount(0): media on nas -> smb://nas/media/`.
pub fn mounted(host: &str) -> Vec<Mounted> {
    let Ok(out) = Command::new("gio").args(["mount", "-l"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| parse_gio_line(line, host))
        .collect()
}

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

pub fn mount(host: &str, share: &str, user: &str, password: &str) -> Result<(), String> {
    let mut child = Command::new("gio")
        .args(["mount", &uri(host, share)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(gio_missing)?;
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
    } else {
        Err(msg)
    }
}

pub fn unmount(_host: &str, m: &Mounted) -> Result<(), String> {
    let out = Command::new("gio")
        .args(["mount", "-u", &m.location])
        .output()
        .map_err(gio_missing)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn open(_host: &str, m: &Mounted) -> Result<(), String> {
    Command::new("gio")
        .args(["open", &m.location])
        .spawn()
        .map(|_| ())
        .map_err(gio_missing)
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
}
