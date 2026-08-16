#!/usr/bin/env bash
set -euo pipefail

# deploy_and_collect_vf2.sh
# 上传内核镜像到 VF2、备份现有引导、可选重启并收集 boot dmesg/journal，最后把日志拉回本地。
# 用法:
#   ./deploy_and_collect_vf2.sh <local-image> dy@192.168.28.139 [--remote-boot /boot/vmlinuz] [--no-reboot]
# 注意：脚本不会自动使用明文密码。若要免交互请配置 SSH key 或安装并使用 sshpass（不推荐）。

LOCAL_IMAGE="${1:-}"
REMOTE_USER_HOST="${2:-}"
REMOTE_BOOT_PATH="/boot/vmlinuz"
NO_REBOOT=0

shift 2 || true
while [ "$#" -gt 0 ]; do
  case "$1" in
    --remote-boot)
      REMOTE_BOOT_PATH="$2"; shift 2;;
    --no-reboot)
      NO_REBOOT=1; shift;;
    --help|-h)
      echo "Usage: $0 <local-image> user@host [--remote-boot /boot/vmlinuz] [--no-reboot]"; exit 0;;
    *) echo "Unknown arg: $1"; exit 1;;
  esac
done

if [ -z "$LOCAL_IMAGE" ] || [ -z "$REMOTE_USER_HOST" ]; then
  echo "Error: missing arguments." >&2
  echo "Usage: $0 <local-image> user@host [--remote-boot /boot/vmlinuz] [--no-reboot]" >&2
  exit 2
fi

if [ ! -f "$LOCAL_IMAGE" ]; then
  echo "Local image not found: $LOCAL_IMAGE" >&2
  exit 3
fi

# 远端临时路径与备份命名
TIMESTAMP=$(date +%s)
REMOTE_TMP_DIR="/home/${REMOTE_USER_HOST%%@*}/kernel_upload_${TIMESTAMP}"
REMOTE_TMP_PATH="$REMOTE_TMP_DIR/$(basename "$LOCAL_IMAGE")"
REMOTE_BACKUP_DIR="/boot/backup_$TIMESTAMP"
REMOTE_DMESG="/home/${REMOTE_USER_HOST%%@*}/dmesg_after_boot_${TIMESTAMP}.txt"
REMOTE_JOURNAL="/home/${REMOTE_USER_HOST%%@*}/journal_boot_${TIMESTAMP}.log"
ROLLBACK_SCRIPT="/home/${REMOTE_USER_HOST%%@*}/rollback_vmlinuz_${TIMESTAMP}.sh"

echo "Uploading $LOCAL_IMAGE -> $REMOTE_USER_HOST:$REMOTE_TMP_PATH"
ssh -o StrictHostKeyChecking=accept-new "$REMOTE_USER_HOST" "mkdir -p '$REMOTE_TMP_DIR'"
scp "$LOCAL_IMAGE" "$REMOTE_USER_HOST":"$REMOTE_TMP_PATH"

echo "On remote: backing up current boot image (if exists) and installing new image"
ssh "$REMOTE_USER_HOST" bash -s <<EOF
set -e
if [ -f "$REMOTE_BOOT_PATH" ]; then
  sudo mkdir -p "$REMOTE_BACKUP_DIR"
  sudo cp -a "$REMOTE_BOOT_PATH" "$REMOTE_BACKUP_DIR/"
  echo "Backed up existing $REMOTE_BOOT_PATH to $REMOTE_BACKUP_DIR/"
else
  echo "No existing $REMOTE_BOOT_PATH found; skipping backup"
fi
# Move uploaded image into place (use sudo to write /boot)
sudo cp -a "$REMOTE_TMP_PATH" "$REMOTE_BOOT_PATH"
sudo sync
# Write rollback helper
cat > "$ROLLBACK_SCRIPT" <<'RB'
#!/usr/bin/env bash
set -e
if [ -d "$REMOTE_BACKUP_DIR" ]; then
  sudo cp -a "$REMOTE_BACKUP_DIR/$(basename "$REMOTE_BOOT_PATH")" "$REMOTE_BOOT_PATH"
  sudo sync
  echo "Restored $REMOTE_BOOT_PATH from backup"
else
  echo "No backup found at $REMOTE_BACKUP_DIR"
fi
RB
sudo chmod +x "$ROLLBACK_SCRIPT"
echo "Rollback helper created at $ROLLBACK_SCRIPT"
EOF

if [ "$NO_REBOOT" -eq 0 ]; then
  echo "Rebooting remote host: $REMOTE_USER_HOST"
  ssh "$REMOTE_USER_HOST" "sudo reboot" || true
  echo "Waiting for host to come back up..."
  # 等待远端重启并上线（最多 300s）
  for i in {1..60}; do
    sleep 5
    if ssh -o ConnectTimeout=5 -o BatchMode=yes "$REMOTE_USER_HOST" true 2>/dev/null; then
      echo "Host is back online"
      break
    fi
    echo -n "."
  done
fi

# 采集 dmesg 与 journal（在远端已重启并可连接时执行）
echo "Collecting dmesg and journal on remote"
ssh "$REMOTE_USER_HOST" "dmesg -T | sudo tee '$REMOTE_DMESG' > /dev/null"
ssh "$REMOTE_USER_HOST" "sudo journalctl -b > '$REMOTE_JOURNAL' || true"

echo "Downloading logs to current directory"
scp "$REMOTE_USER_HOST":"$REMOTE_DMESG" ./
scp "$REMOTE_USER_HOST":"$REMOTE_JOURNAL" ./ || true

echo "Done. Remote logs saved as: $(basename "$REMOTE_DMESG") and $(basename "$REMOTE_JOURNAL")"
echo "If you need to rollback, SSH to the board and run: sudo $ROLLBACK_SCRIPT"

exit 0
