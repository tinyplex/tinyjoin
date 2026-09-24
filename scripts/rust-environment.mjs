import {existsSync, statSync} from 'node:fs';
import {spawnSync} from 'node:child_process';
import {delimiter, dirname, resolve} from 'node:path';

// Resolves the environment for running the toolchain pinned by
// rust-toolchain.toml, even when another Rust install, such as Homebrew's,
// comes first on PATH.
export function resolveRustEnvironment(root) {
  const pathKey =
    Object.keys(process.env).find((key) => key.toLowerCase() === 'path') ??
    'PATH';
  const rustupExecutable = findExecutable(
    pathKey,
    process.platform === 'win32' ? ['rustup.exe', 'rustup'] : ['rustup'],
  );
  // Prefer rustup's own cargo and rustc proxies: they select the pinned
  // toolchain and set the library path its tools need, such as the shared
  // libLLVM that rust-lld loads on macOS. The resolved toolchain binaries are the
  // fallback when the proxies do not sit beside rustup.
  const rustupProxyDirectories =
    rustupExecutable &&
    ['cargo', 'rustc'].every((tool) => isRustupProxy(tool, rustupExecutable))
      ? [dirname(rustupExecutable)]
      : [];
  const rustupTools =
    rustupExecutable && rustupProxyDirectories.length === 0
      ? [
          capture(root, rustupExecutable, ['which', 'rustc']),
          capture(root, rustupExecutable, ['which', 'cargo']),
        ]
      : [];
  const rustupToolchainDirectories =
    rustupTools.length === 2 &&
    rustupTools.every((tool) => tool.ok && existsSync(tool.output))
      ? [...new Set(rustupTools.map((tool) => dirname(tool.output)))]
      : [];
  const rustDirectories = [
    ...rustupProxyDirectories,
    ...rustupToolchainDirectories,
  ];
  const rustEnvironment = rustDirectories.length > 0
    ? {
        ...process.env,
        [pathKey]: [...rustDirectories, process.env[pathKey]]
          .filter(Boolean)
          .join(delimiter),
      }
    : process.env;
  return {rustDirectories, rustEnvironment};
}

function capture(root, command, args, environment = process.env) {
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: 'utf8',
    env: environment,
  });
  return {
    ok: result.status === 0,
    output: result.status === 0 ? result.stdout.trim() : '',
  };
}

function isRustupProxy(tool, rustup) {
  const name = process.platform === 'win32' ? `${tool}.exe` : tool;
  try {
    // Proxies are symbolic or hard links to rustup itself.
    const proxy = statSync(resolve(dirname(rustup), name));
    const target = statSync(rustup);
    return proxy.dev === target.dev && proxy.ino === target.ino;
  } catch {
    return false;
  }
}

function findExecutable(pathKey, names) {
  const path = process.env[pathKey];
  if (!path) {
    return undefined;
  }
  for (const directory of path.split(delimiter)) {
    if (!directory) {
      continue;
    }
    for (const name of names) {
      const candidate = resolve(directory.replace(/^"|"$/g, ''), name);
      if (existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}
