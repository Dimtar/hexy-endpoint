//! Root helper that lets Hexy Endpoint mount SMB shares with the kernel's
//! CIFS client on Linux, which an ordinary user is not allowed to do.
//!
//! It is installed by the .deb and .rpm packages and run through `pkexec`,
//! which the bundled polkit policy permits for the logged-in desktop user.
//! Because it runs as root on behalf of an unprivileged caller, it is kept
//! deliberately small, uses only the standard library, and trusts nothing
//! the caller sends:
//!
//! - It only ever mounts onto `/run/hexy-endpoint/<caller uid>/<share>`.
//!   Every directory on that path is created by, and writable only by, root,
//!   so the caller cannot swap in a symlink to redirect a mount elsewhere.
//! - Host, share and username are checked against strict rules before use,
//!   and none can smuggle in extra mount options.
//! - Mounts are always `nosuid,nodev` and owned by the caller.
//! - The password arrives on stdin and is handed to `mount.cifs` through
//!   its environment, never on a command line.
//! - It only unmounts inside the caller's own directory.
//!
//! Usage (as root, with PKEXEC_UID set by pkexec):
//!   hexy-endpoint-mount-helper mount <host> <share> <username>   (password on stdin)
//!   hexy-endpoint-mount-helper unmount <share>

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("hexy-endpoint-mount-helper is only used on Linux");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = helper::run(&args) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod helper {
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    const BASE: &str = "/run/hexy-endpoint";
    const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

    pub fn run(args: &[String]) -> Result<(), String> {
        let running_as = fs::metadata("/proc/self")
            .map(|m| m.uid())
            .map_err(|e| format!("cannot read /proc/self: {e}"))?;
        if running_as != 0 {
            return Err("this helper must be run as root, through pkexec".into());
        }
        // Set by pkexec to the user who asked; that user owns the mounts.
        let caller: u32 = std::env::var("PKEXEC_UID")
            .ok()
            .and_then(|v| v.parse().ok())
            .ok_or("PKEXEC_UID is not set; run this helper through pkexec")?;

        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            ["mount", host, share, user] => mount(caller, host, share, user),
            ["unmount", share] => unmount(caller, share),
            _ => Err("usage: mount <host> <share> <username> | unmount <share>".into()),
        }
    }

    pub fn valid_host(host: &str) -> bool {
        !host.is_empty()
            && host.len() <= 253
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    }

    /// A share name is used as a single directory name and as the last part
    /// of `//host/share`, so it must not be able to climb out of either.
    pub fn valid_share(share: &str) -> bool {
        !share.is_empty()
            && share.len() <= 80
            && share != "."
            && share != ".."
            && !share
                .chars()
                .any(|c| c == '/' || c == '\\' || c.is_control())
    }

    /// The username goes into the mount option string, where a comma would
    /// start a new option.
    pub fn valid_user(user: &str) -> bool {
        !user.is_empty() && user.len() <= 256 && !user.chars().any(|c| c == ',' || c.is_control())
    }

    fn target_dir(caller: u32, share: &str) -> PathBuf {
        Path::new(BASE).join(caller.to_string()).join(share)
    }

    /// Creates a directory if needed and insists that what is there is a
    /// real directory owned by root, not something planted in its place.
    fn root_owned_dir(path: &Path) -> Result<(), String> {
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
        let meta = fs::symlink_metadata(path)
            .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
        if !meta.file_type().is_dir() || meta.uid() != 0 {
            return Err(format!("{} is not a root-owned directory", path.display()));
        }
        Ok(())
    }

    /// Undoes the octal escapes the kernel uses in /proc/self/mounts
    /// (`\040` for a space, and so on).
    pub fn unescape(field: &str) -> String {
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

    fn is_mounted(target: &Path) -> bool {
        let Ok(table) = fs::read_to_string("/proc/self/mounts") else {
            return false;
        };
        table.lines().any(|line| {
            line.split(' ')
                .nth(1)
                .is_some_and(|field| Path::new(&unescape(field)) == target)
        })
    }

    fn find_program(names: &[&str]) -> Option<&'static str> {
        const DIRS: [&str; 4] = ["/usr/sbin", "/sbin", "/usr/bin", "/bin"];
        for dir in DIRS {
            for name in names {
                let path = format!("{dir}/{name}");
                if Path::new(&path).is_file() {
                    return Some(Box::leak(path.into_boxed_str()));
                }
            }
        }
        None
    }

    fn mount(caller: u32, host: &str, share: &str, user: &str) -> Result<(), String> {
        if !valid_host(host) {
            return Err("invalid server address".into());
        }
        if !valid_share(share) {
            return Err("invalid share name".into());
        }
        if !valid_user(user) {
            return Err("invalid username".into());
        }
        let mut password = String::new();
        std::io::stdin()
            .take(4096)
            .read_to_string(&mut password)
            .map_err(|e| format!("cannot read the password: {e}"))?;
        let password = password.trim_end_matches(['\n', '\r']);
        if password.contains('\0') {
            return Err("invalid password".into());
        }

        let mount_cifs = find_program(&["mount.cifs"])
            .ok_or("mount.cifs was not found; install the cifs-utils package")?;

        let target = target_dir(caller, share);
        root_owned_dir(Path::new(BASE))?;
        root_owned_dir(&Path::new(BASE).join(caller.to_string()))?;
        if is_mounted(&target) {
            return Ok(());
        }
        root_owned_dir(&target)?;

        let options = format!(
            "username={user},uid={caller},forceuid,\
             file_mode=0600,dir_mode=0700,nosuid,nodev,iocharset=utf8"
        );
        let output = Command::new(mount_cifs)
            .arg(format!("//{host}/{share}"))
            .arg(&target)
            .arg("-o")
            .arg(options)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .env("PASSWD", password)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run mount.cifs: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let _ = fs::remove_dir(&target);
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if message.is_empty() {
            "mount.cifs failed".into()
        } else {
            message
        })
    }

    fn unmount(caller: u32, share: &str) -> Result<(), String> {
        if !valid_share(share) {
            return Err("invalid share name".into());
        }
        let target = target_dir(caller, share);
        if is_mounted(&target) {
            let umount = find_program(&["umount"]).ok_or("umount was not found")?;
            let output = Command::new(umount)
                .arg(&target)
                .env_clear()
                .env("PATH", SAFE_PATH)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("cannot run umount: {e}"))?;
            if !output.status.success() {
                let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(if message.is_empty() {
                    "umount failed".into()
                } else {
                    message
                });
            }
        }
        let _ = fs::remove_dir(&target);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hosts() {
            assert!(valid_host("192.168.1.10"));
            assert!(valid_host("fable.local"));
            assert!(!valid_host(""));
            assert!(!valid_host("-o"));
            assert!(!valid_host("nas/share"));
            assert!(!valid_host("nas,port=1"));
        }

        #[test]
        fn shares() {
            assert!(valid_share("Stuff"));
            assert!(valid_share("Virtualization Install Media"));
            assert!(!valid_share(".."));
            assert!(!valid_share("a/b"));
            assert!(!valid_share("a\\b"));
            assert!(!valid_share("a\nb"));
            assert!(!valid_share(""));
        }

        #[test]
        fn users() {
            assert!(valid_user("daniel"));
            assert!(valid_user("HOME\\daniel"));
            assert!(!valid_user("daniel,uid=0"));
            assert!(!valid_user("a\nb"));
            assert!(!valid_user(""));
        }

        #[test]
        fn targets_stay_inside_the_callers_directory() {
            assert_eq!(
                target_dir(1000, "My Docs"),
                Path::new("/run/hexy-endpoint/1000/My Docs")
            );
        }

        #[test]
        fn unescapes_mount_table_fields() {
            assert_eq!(unescape("/run/x/My\\040Docs"), "/run/x/My Docs");
            assert_eq!(unescape("plain"), "plain");
            assert_eq!(unescape("back\\134slash"), "back\\slash");
            assert_eq!(unescape("trailing\\"), "trailing\\");
        }
    }
}
