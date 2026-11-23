// Tauri / Cargo 命令的跨平台入口：npm scripts 统一调用本文件。
// POSIX 下加载 ~/.cargo/env；Windows 下直接调用项目内 CLI，不改写 PATH。
import { spawnSync } from 'node:child_process';
import { platform } from 'node:os';
import { fileURLToPath } from 'node:url';

const [command, ...args] = process.argv.slice(2);
if (!command) {
  console.error('用法: node scripts/tauri.mjs <command> [args...]');
  process.exit(1);
}

const isWindows = platform() === 'win32';
let child;

if (!isWindows) {
  child = spawnSync(
    'bash',
    ['-c', '. "$HOME/.cargo/env" 2>/dev/null || true; exec "$@"', 'bash', command, ...args],
    { stdio: 'inherit' },
  );
} else if (command === 'tauri') {
  // 绕开 cmd 的 PATH 解析，直接使用本地安装的 CLI。
  const cli = fileURLToPath(new URL('../node_modules/@tauri-apps/cli/tauri.js', import.meta.url));
  child = spawnSync(process.execPath, [cli, ...args], { stdio: 'inherit' });
} else {
  // cargo 由用户终端的 PATH 解析（rustup 安装时写入）。
  child = spawnSync(command, args, { stdio: 'inherit', shell: true });
}

if (child.error) {
  console.error(child.error.message);
  process.exit(1);
}
process.exit(child.status ?? 1);
