#!/bin/sh
set -eu
umask 022
status_dir=/opt/frigate/backup-status
mkdir -p "$status_dir"
exec 9>"$status_dir/upload.lock"
flock -n 9 || exit 0
result=0
rclone copy /opt/frigate/storage/recordings gdrive:Tapo-Frigate/recordings \
  --min-age 1m --log-level NOTICE || result=$?
temp=$(mktemp "$status_dir/.latest.XXXXXX")
trap 'rm -f "$temp"' EXIT
if [ "$result" -eq 0 ]; then
  printf 'Lượt sao lưu hoàn tất · %s\n' "$(date -Iseconds)" > "$temp"
else
  printf 'Lượt sao lưu lỗi (mã %s) · %s\n' "$result" "$(date -Iseconds)" > "$temp"
fi
chmod 644 "$temp"
mv -f "$temp" "$status_dir/latest.txt"
exit "$result"
