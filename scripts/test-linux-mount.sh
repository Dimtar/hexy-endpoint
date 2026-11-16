#!/usr/bin/env bash
# End-to-end test of the Linux mount helper against a real Samba server.
# Run on a throwaway machine (CI): it installs a Samba config and a user.
#
# Usage: scripts/test-linux-mount.sh [path-to-helper]
set -euo pipefail

helper="${1:-/usr/lib/hexy-endpoint/hexy-endpoint-mount-helper}"
user="$(id -un)"
uid="$(id -u)"
password="hexy-test-$RANDOM-$RANDOM"
share="Test Share"
mounts="/run/hexy-endpoint/$uid"

fail() { echo "FAIL: $*" >&2; exit 1; }
as_caller() { sudo PKEXEC_UID="$uid" "$helper" "$@"; }

echo "== Samba server with one share"
sudo mkdir -p /srv/hexy-test
sudo chown "$user" /srv/hexy-test
echo "hello from the server" > /srv/hexy-test/hello.txt
sudo tee /etc/samba/smb.conf > /dev/null <<CONF
[global]
  workgroup = WORKGROUP
  server role = standalone server
  security = user
  map to guest = never
[$share]
  path = /srv/hexy-test
  read only = no
  comment = For the mount test
CONF
printf '%s\n%s\n' "$password" "$password" | sudo smbpasswd -a -s "$user" > /dev/null
sudo systemctl restart smbd
sleep 2
sudo modprobe cifs || fail "the cifs kernel module is not available on this machine"

echo "== bad input is refused"
as_caller mount 127.0.0.1 "../etc" "$user" <<< "$password" && fail "accepted a share with a slash"
as_caller mount 127.0.0.1 ".." "$user" <<< "$password" && fail "accepted .. as a share"
as_caller mount "127.0.0.1/x" "$share" "$user" <<< "$password" && fail "accepted a bad host"
as_caller mount 127.0.0.1 "$share" "$user,uid=0" <<< "$password" && fail "accepted a username with a comma"
sudo "$helper" mount 127.0.0.1 "$share" "$user" <<< "$password" && fail "ran without PKEXEC_UID"
"$helper" mount 127.0.0.1 "$share" "$user" <<< "$password" && fail "ran without root"

echo "== a wrong password is refused and leaves nothing behind"
as_caller mount 127.0.0.1 "$share" "$user" <<< "not-the-password" && fail "mounted with a wrong password"
[ ! -e "$mounts/$share" ] || fail "left a directory behind after a failed mount"

echo "== mount"
as_caller mount 127.0.0.1 "$share" "$user" <<< "$password"
grep -q "hello from the server" "$mounts/$share/hello.txt" || fail "cannot read the share"
echo "written by the client" > "$mounts/$share/from-client.txt"
[ -f /srv/hexy-test/from-client.txt ] || fail "a write did not reach the server"
[ "$(stat -c %u "$mounts/$share")" = "$uid" ] || fail "the mount is not owned by the caller"
[ "$(stat -c %u "$mounts")" = "0" ] || fail "the mount directory's parent is not owned by root"
grep " $(printf '%s' "$mounts/$share" | sed 's/ /\\\\040/g') cifs " /proc/self/mounts | grep -q nosuid || fail "not mounted nosuid"
as_caller mount 127.0.0.1 "$share" "$user" <<< "$password" || fail "mounting twice should be a no-op"

echo "== unmount"
as_caller unmount "$share"
[ ! -e "$mounts/$share" ] || fail "the mount directory is still there"
grep -q " cifs " /proc/self/mounts && fail "still mounted"
as_caller unmount "$share" || fail "unmounting twice should be a no-op"

echo "PASS"
