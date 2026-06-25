#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const KIND_CLI = "difflore";
const KIND_HOOK = "difflore-hook";
const NOOP_OUTPUT = "{\"continue\":true}";
const CLI_VERSION_RE = /(?:difflore(?:-cli)?|DiffLore)\s+v?([0-9]+\.[0-9]+\.[0-9]+(?:[-+][^\s]+)?)/i;
const HELP_RE = /DiffLore|Source-backed team rules|difflore\b/i;

function env(options = {}) {
  return options.env || process.env;
}

function commandName(kind) {
  if (kind === KIND_HOOK) return "difflore-hook";
  if (kind === KIND_CLI) return "difflore";
  throw new Error(`Unknown DiffLore runtime kind: ${kind}`);
}

function binaryName(kind) {
  const name = commandName(kind);
  return process.platform === "win32" ? `${name}.exe` : name;
}

function candidateNames(kind) {
  return [binaryName(kind)];
}

function pluginRoot(options = {}) {
  return path.resolve(options.pluginRoot || env(options).PLUGIN_ROOT || path.join(__dirname, ".."));
}

function looksLikeRepoRoot(candidate) {
  return fs.existsSync(path.join(candidate, "Cargo.toml")) && fs.existsSync(path.join(candidate, "crates"));
}

function repoRoot(options = {}) {
  if (options.repoRoot) return path.resolve(options.repoRoot);
  const root = pluginRoot(options);
  for (const candidate of [path.join(root, ".."), path.join(root, "..", "..")]) {
    if (looksLikeRepoRoot(candidate)) return path.resolve(candidate);
  }
  return path.resolve(path.join(root, ".."));
}

function manifestCandidates(options = {}) {
  const root = pluginRoot(options);
  return [
    path.join(root, ".codex-plugin", "plugin.json"),
    path.join(root, "..", ".codex-plugin", "plugin.json")
  ];
}

function expectedVersion(options = {}) {
  if (Object.prototype.hasOwnProperty.call(options, "expectedVersion")) {
    return options.expectedVersion;
  }
  if (env(options).DIFFLORE_PLUGIN_EXPECTED_VERSION) {
    return env(options).DIFFLORE_PLUGIN_EXPECTED_VERSION;
  }
  for (const candidate of manifestCandidates(options)) {
    try {
      const manifest = JSON.parse(fs.readFileSync(candidate, "utf8"));
      if (manifest.version) return manifest.version;
    } catch (_error) {
      // Try the next manifest location.
    }
  }
  return undefined;
}

function defaultPluginDataDir(envVars = process.env) {
  if (envVars.DIFFLORE_PLUGIN_DATA) return envVars.DIFFLORE_PLUGIN_DATA;
  if (envVars.PLUGIN_DATA) return envVars.PLUGIN_DATA;
  const home = os.homedir();
  if (process.platform === "win32") {
    return path.join(envVars.LOCALAPPDATA || home, "difflore", "codex-plugin");
  }
  return path.join(home, ".difflore", "codex-plugin");
}

function pluginDataDir(options = {}) {
  return path.resolve(options.pluginData || defaultPluginDataDir(env(options)));
}

function runtimeMetadataPath(options = {}) {
  return path.join(pluginDataDir(options), "runtime.json");
}

function diffloreHome(options = {}) {
  return path.resolve(env(options).DIFFLORE_HOME || path.join(os.homedir(), ".difflore"));
}

function installRecordPath(options = {}) {
  return path.resolve(options.installRecordPath || path.join(diffloreHome(options), "mcp.json"));
}

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (_error) {
    return null;
  }
}

function readMetadata(options = {}) {
  return readJson(runtimeMetadataPath(options));
}

function readInstallRecord(options = {}) {
  const recordPath = installRecordPath(options);
  const record = readJson(recordPath);
  if (!record || typeof record !== "object") return null;
  if (typeof record.command !== "string" || record.command.trim() === "") return null;
  const args = Array.isArray(record.args) ? record.args.filter((arg) => typeof arg === "string") : [];
  return {
    path: recordPath,
    command: record.command,
    args
  };
}

function isExecutable(candidate, options = {}) {
  if (!candidate) return false;
  if (options.isExecutable) return options.isExecutable(candidate);
  try {
    const stat = fs.statSync(candidate);
    if (!stat.isFile()) return false;
    fs.accessSync(candidate, fs.constants.X_OK);
    return true;
  } catch (_error) {
    return false;
  }
}

function pathEnvValue(envVars) {
  return envVars.PATH || envVars.Path || envVars.path || "";
}

function pathCandidates(kind, envVars = process.env) {
  const names = candidateNames(kind);
  return pathEnvValue(envVars)
    .split(path.delimiter)
    .filter(Boolean)
    .flatMap((dir) => names.map((name) => path.join(dir, name)));
}

function commonInstallDirs(options = {}) {
  const envVars = env(options);
  const home = envVars.HOME || envVars.USERPROFILE || os.homedir();
  const dirs = [
    path.join(home, ".difflore", "bin"),
    path.join(home, ".cargo", "bin"),
    path.join(home, ".local", "bin"),
    "/usr/local/bin"
  ];
  if (process.platform === "win32") {
    for (const dir of [
      envVars.LOCALAPPDATA && path.join(envVars.LOCALAPPDATA, "Programs", "difflore"),
      envVars.LOCALAPPDATA && path.join(envVars.LOCALAPPDATA, "difflore", "bin"),
      envVars.ProgramFiles && path.join(envVars.ProgramFiles, "difflore-cli"),
      envVars.ProgramFiles && path.join(envVars.ProgramFiles, "difflore"),
      envVars["ProgramFiles(x86)"] && path.join(envVars["ProgramFiles(x86)"], "difflore-cli")
    ]) {
      if (dir) dirs.push(dir);
    }
  }
  return [...new Set(dirs)];
}

function repoCandidates(kind, options = {}) {
  const root = repoRoot(options);
  return [
    path.join(root, "target", "release", binaryName(kind)),
    path.join(root, "target", "debug", binaryName(kind))
  ];
}

function adjacentHookCandidate(cliPath) {
  if (!cliPath) return null;
  return path.join(path.dirname(cliPath), binaryName(KIND_HOOK));
}

function candidateEntries(kind, options = {}) {
  const envVars = env(options);
  const entries = [];
  const explicitKey = kind === KIND_HOOK ? "DIFFLORE_HOOK_BINARY" : "DIFFLORE_BINARY";
  if (envVars[explicitKey]) {
    entries.push({ source: "explicit", path: envVars[explicitKey], kind });
  }

  const metadata = readMetadata(options);
  const metadataPath = kind === KIND_HOOK
    ? metadata?.binaries?.diffloreHook
    : metadata?.binaries?.difflore;
  if (metadataPath) {
    entries.push({ source: "metadata", path: metadataPath, kind });
  }

  const record = readInstallRecord(options);
  if (record?.command) {
    if (kind === KIND_CLI) {
      entries.push({ source: "install-record", path: record.command, kind });
    } else {
      entries.push({ source: "install-record-adjacent", path: adjacentHookCandidate(record.command), kind });
    }
  }

  for (const candidate of repoCandidates(kind, options)) {
    entries.push({ source: "repo", path: candidate, kind });
  }
  for (const dir of commonInstallDirs(options)) {
    for (const name of candidateNames(kind)) {
      entries.push({ source: "common-install", path: path.join(dir, name), kind });
    }
  }
  for (const candidate of pathCandidates(kind, envVars)) {
    entries.push({ source: "path", path: candidate, kind });
  }

  const seen = new Set();
  return entries
    .filter((entry) => entry.path)
    .map((entry) => ({ ...entry, path: path.resolve(entry.path) }))
    .filter((entry) => {
      const key = `${entry.kind}:${entry.path.toLowerCase()}`;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
}

function runProbe(candidate, args, options = {}) {
  if (options.runProbe) return options.runProbe(candidate, args);
  const result = spawnSync(candidate, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 3000
  });
  if (result.error) {
    return { ok: false, reason: result.error.message, stdout: "", stderr: "" };
  }
  return {
    ok: result.status === 0,
    status: result.status,
    stdout: result.stdout || "",
    stderr: result.stderr || "",
    reason: result.status === 0
      ? undefined
      : (result.stderr || result.stdout || `exit ${result.status}`).trim()
  };
}

function parseCliVersion(output) {
  const match = CLI_VERSION_RE.exec(output || "");
  return match ? match[1] : undefined;
}

function inspectCli(candidate, options = {}) {
  if (options.inspectCommand) return options.inspectCommand(KIND_CLI, candidate);
  const versionProbe = runProbe(candidate, ["--version"], options);
  const version = parseCliVersion(`${versionProbe.stdout}\n${versionProbe.stderr}`);
  const helpProbe = runProbe(candidate, ["--help"], options);
  const help = `${helpProbe.stdout}\n${helpProbe.stderr}`;
  if (helpProbe.ok && HELP_RE.test(help)) {
    return {
      ok: true,
      version,
      output: (versionProbe.stdout || versionProbe.stderr || helpProbe.stdout || "").trim()
    };
  }
  if (version) {
    return {
      ok: true,
      version,
      output: (versionProbe.stdout || versionProbe.stderr || "").trim()
    };
  }
  return {
    ok: false,
    reason: helpProbe.reason || versionProbe.reason || "not a difflore CLI binary"
  };
}

function inspectHook(candidate, entry, options = {}) {
  if (options.inspectCommand) return options.inspectCommand(KIND_HOOK, candidate);
  const base = path.basename(candidate).toLowerCase();
  const expected = binaryName(KIND_HOOK).toLowerCase();
  if (entry.source !== "explicit" && base !== expected) {
    return {
      ok: false,
      reason: `${candidate}: expected ${expected} next to the installed difflore binary`
    };
  }
  return { ok: true };
}

function versionMismatchMessage(candidate, expected, actual) {
  return `${candidate}: found difflore ${actual}, expected ${expected}`;
}

function inspectCandidate(entry, expected, options = {}) {
  const exists = fs.existsSync(entry.path);
  const executable = isExecutable(entry.path, options);
  if (!executable) {
    return {
      ...entry,
      exists,
      executable: false,
      ok: false,
      reason: exists ? "not executable" : "not found"
    };
  }

  const inspected = entry.kind === KIND_HOOK
    ? inspectHook(entry.path, entry, options)
    : inspectCli(entry.path, options);
  let ok = inspected.ok;
  let reason = inspected.reason;
  const allowMismatch = env(options).DIFFLORE_ALLOW_VERSION_MISMATCH === "1";
  if (
    ok &&
    entry.kind === KIND_CLI &&
    expected &&
    inspected.version &&
    inspected.version !== expected &&
    !allowMismatch
  ) {
    ok = false;
    reason = versionMismatchMessage(entry.path, expected, inspected.version);
  }
  return {
    ...entry,
    exists: true,
    executable: true,
    ok,
    version: inspected.version,
    output: inspected.output,
    reason: ok ? undefined : reason
  };
}

function inspectRuntime(options = {}) {
  const expected = expectedVersion(options);
  const cliCandidates = candidateEntries(KIND_CLI, options).map((entry) =>
    inspectCandidate(entry, expected, options)
  );
  const hookCandidates = candidateEntries(KIND_HOOK, options).map((entry) =>
    inspectCandidate(entry, expected, options)
  );
  return {
    expectedVersion: expected,
    pluginRoot: pluginRoot(options),
    pluginData: pluginDataDir(options),
    metadataPath: runtimeMetadataPath(options),
    installRecordPath: installRecordPath(options),
    selected: {
      difflore: cliCandidates.find((candidate) => candidate.ok),
      diffloreHook: hookCandidates.find((candidate) => candidate.ok)
    },
    candidates: {
      difflore: cliCandidates,
      diffloreHook: hookCandidates
    }
  };
}

function candidatesForKind(kind, status) {
  return kind === KIND_HOOK ? status.candidates.diffloreHook : status.candidates.difflore;
}

function rejectedCandidateMessages(kind, status) {
  return candidatesForKind(kind, status)
    .filter((candidate) => candidate.exists && !candidate.ok && candidate.reason)
    .map((candidate) => `${candidate.source} ${candidate.path}: ${candidate.reason}`);
}

function runtimeMissingMessage(kind, status) {
  const label = kind === KIND_HOOK ? "difflore-hook hook shim" : "difflore CLI";
  const envName = kind === KIND_HOOK ? "DIFFLORE_HOOK_BINARY" : "DIFFLORE_BINARY";
  const rejected = rejectedCandidateMessages(kind, status);
  const checked = [
    envName,
    status.metadataPath,
    status.installRecordPath,
    "repo target/{release,debug}",
    "common install dirs",
    "PATH fallback"
  ];
  return [
    `Unable to find an installed ${label} for the DiffLore Codex plugin.`,
    `Checked: ${checked.join("; ")}.`,
    "Install DiffLore first, then run `difflore agents install` so ~/.difflore/mcp.json points at the active CLI.",
    `For local development, set ${envName} to an absolute path.`,
    rejected.length ? `Rejected candidates: ${rejected.join(" | ")}` : "No executable candidates were found."
  ].join(" ");
}

function ensureBinary(kind, options = {}) {
  const status = inspectRuntime(options);
  const selected = kind === KIND_HOOK ? status.selected.diffloreHook : status.selected.difflore;
  if (selected) return selected.path;
  throw new Error(runtimeMissingMessage(kind, status));
}

function realpathOrSelf(file) {
  try {
    return fs.realpathSync(file);
  } catch (_error) {
    return file;
  }
}

function writeMetadata(metadata, options = {}) {
  const file = runtimeMetadataPath(options);
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o755 });
  fs.writeFileSync(file, `${JSON.stringify(metadata, null, 2)}\n`, { mode: 0o600 });
  return file;
}

function recordRuntime(options = {}) {
  const status = inspectRuntime(options);
  const cli = status.selected.difflore;
  const hook = status.selected.diffloreHook;
  if (!cli) throw new Error(runtimeMissingMessage(KIND_CLI, status));
  if (!hook) throw new Error(runtimeMissingMessage(KIND_HOOK, status));

  const metadata = {
    version: cli.version || status.expectedVersion || null,
    binaries: {
      difflore: realpathOrSelf(cli.path),
      diffloreHook: realpathOrSelf(hook.path)
    },
    sources: {
      difflore: cli.source,
      diffloreHook: hook.source
    },
    recordedAt: new Date().toISOString()
  };
  writeMetadata(metadata, options);
  return metadata;
}

function runtimeEnv(options = {}) {
  return {
    ...process.env,
    ...env(options),
    DIFFLORE_PLUGIN_ROOT: pluginRoot(options),
    DIFFLORE_PLUGIN_DATA: pluginDataDir(options)
  };
}

function runBinary(kind, args, options = {}) {
  const bin = realpathOrSelf(options.binary || ensureBinary(kind, options));
  const result = spawnSync(bin, args, {
    stdio: "inherit",
    env: runtimeEnv(options),
    ...options.spawnOptions
  });
  if (result.error) throw result.error;
  return result.status === null ? 1 : result.status;
}

function humanStatus(status) {
  const selectedCli = status.selected.difflore
    ? `${status.selected.difflore.source} ${status.selected.difflore.path}`
    : "none";
  const selectedHook = status.selected.diffloreHook
    ? `${status.selected.diffloreHook.source} ${status.selected.diffloreHook.path}`
    : "none";
  const lines = [
    `expected: ${status.expectedVersion || "unknown"}`,
    `plugin_root: ${status.pluginRoot}`,
    `plugin_data: ${status.pluginData}`,
    `metadata: ${status.metadataPath}`,
    `install_record: ${status.installRecordPath}`,
    `selected_difflore: ${selectedCli}`,
    `selected_difflore_hook: ${selectedHook}`
  ];
  for (const kind of [KIND_CLI, KIND_HOOK]) {
    for (const candidate of candidatesForKind(kind, status)) {
      if (!candidate.exists && !candidate.executable) continue;
      const state = candidate.ok ? "ok" : "rejected";
      lines.push(`${state}: ${kind} ${candidate.source} ${candidate.path}${candidate.reason ? ` (${candidate.reason})` : ""}`);
    }
  }
  return `${lines.join("\n")}\n`;
}

async function main(argv) {
  const [command = "status", ...args] = argv;
  if (command === "status") {
    const status = inspectRuntime();
    if (args.includes("--json")) {
      process.stdout.write(`${JSON.stringify(status, null, 2)}\n`);
    } else {
      process.stdout.write(humanStatus(status));
    }
    return 0;
  }
  if (command === "record" || command === "install") {
    const metadata = recordRuntime();
    process.stdout.write(`${JSON.stringify(metadata, null, 2)}\n`);
    return 0;
  }
  if (command === "path") {
    const kind = args[0] === KIND_HOOK ? KIND_HOOK : KIND_CLI;
    process.stdout.write(`${ensureBinary(kind)}\n`);
    return 0;
  }
  if (command === "self-test") {
    const cli = ensureBinary(KIND_CLI);
    const hook = ensureBinary(KIND_HOOK);
    process.stdout.write(`difflore=${cli}\ndifflore-hook=${hook}\n`);
    return 0;
  }
  throw new Error(`Unknown DiffLore runtime command: ${command}`);
}

module.exports = {
  KIND_CLI,
  KIND_HOOK,
  NOOP_OUTPUT,
  binaryName,
  candidateEntries,
  commonInstallDirs,
  ensureBinary,
  expectedVersion,
  inspectRuntime,
  installRecordPath,
  isExecutable,
  pathCandidates,
  pluginDataDir,
  pluginRoot,
  recordRuntime,
  repoCandidates,
  repoRoot,
  runBinary,
  runtimeMetadataPath,
  versionMismatchMessage
};

if (require.main === module) {
  main(process.argv.slice(2))
    .then((status) => process.exit(status))
    .catch((error) => {
      process.stderr.write(`${error.message}\n`);
      process.exit(1);
    });
}
