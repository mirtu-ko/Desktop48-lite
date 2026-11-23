// 本地等价 CI：与 .github/workflows/ci.yml 的检查步骤保持一致，跨平台。
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const cwd = fileURLToPath(new URL('..', import.meta.url));
const useShell = process.platform === 'win32';

const steps = [
  ['npm', ['run', 'test']],
  ['npm', ['run', 'typecheck']],
  ['npm', ['run', 'build:web']],
  ['cargo', ['fmt', '--manifest-path', 'src-tauri/Cargo.toml', '--', '--check']],
  [
    'cargo',
    ['clippy', '--manifest-path', 'src-tauri/Cargo.toml', '--all-targets', '--', '-D', 'warnings'],
  ],
  ['cargo', ['test', '--manifest-path', 'src-tauri/Cargo.toml']],
];

for (const [command, args] of steps) {
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', shell: useShell });
  if (result.error) {
    console.error(result.error.message);
    process.exit(1);
  }
  if (result.status !== 0) process.exit(result.status ?? 1);
}
console.log('OK: 本地检查全部通过');
