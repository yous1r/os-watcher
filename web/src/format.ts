/** 将字节数格式化为人类可读字符串。 */
export function formatBytes(bytes: number): string {
  const GB = 1_073_741_824;
  const MB = 1_048_576;
  const KB = 1024;
  if (bytes >= GB) return `${(bytes / GB).toFixed(1)} GB`;
  if (bytes >= MB) return `${(bytes / MB).toFixed(1)} MB`;
  if (bytes >= KB) return `${(bytes / KB).toFixed(1)} KB`;
  return `${bytes} B`;
}

/**
 * 将字节/秒格式化为速率字符串。
 *
 * 速率是小数，且低速率（几百 B/s）也有意义，所以这里保留一位小数而不取整。
 */
export function formatRate(bytesPerSec: number): string {
  if (!Number.isFinite(bytesPerSec) || bytesPerSec <= 0) return "0 B/s";
  const GB = 1_073_741_824;
  const MB = 1_048_576;
  const KB = 1024;
  if (bytesPerSec >= GB) return `${(bytesPerSec / GB).toFixed(2)} GB/s`;
  if (bytesPerSec >= MB) return `${(bytesPerSec / MB).toFixed(1)} MB/s`;
  if (bytesPerSec >= KB) return `${(bytesPerSec / KB).toFixed(1)} KB/s`;
  return `${Math.round(bytesPerSec)} B/s`;
}

/** 汇总所有网卡的上下行速率。 */
export function totalNetworkRates(
  networks: { rx_bytes_per_sec: number; tx_bytes_per_sec: number }[]
): { rx: number; tx: number } {
  return networks.reduce(
    (acc, n) => ({
      rx: acc.rx + (n.rx_bytes_per_sec ?? 0),
      tx: acc.tx + (n.tx_bytes_per_sec ?? 0),
    }),
    { rx: 0, tx: 0 }
  );
}

/** 将秒数格式化为 uptime 字符串。 */
export function formatUptime(secs: number): string {
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  const minutes = Math.floor((secs % 3600) / 60);
  if (days > 0) return `${days}天 ${hours}小时`;
  if (hours > 0) return `${hours}小时 ${minutes}分`;
  return `${minutes}分`;
}

/** 根据使用率百分比返回状态色调。 */
export function usageTone(pct: number): "ok" | "warn" | "crit" {
  if (pct >= 90) return "crit";
  if (pct >= 70) return "warn";
  return "ok";
}

/** 格式化 ISO 时间为 HH:MM:SS。 */
export function formatTime(iso: string): string {
  try {
    return new Date(iso).toLocaleTimeString("zh-CN", { hour12: false });
  } catch {
    return "--:--:--";
  }
}

/** 取一组磁盘中的最高使用率。 */
export function maxDiskUsage(disks: { usage_percent: number }[]): number {
  return disks.reduce((max, d) => Math.max(max, d.usage_percent), 0);
}
