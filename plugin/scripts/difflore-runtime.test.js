#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const test = require("node:test");

const {
  KIND_CLI,
  KIND_HOOK,
  NOOP_OUTPUT,
  binaryName,
  ensureBinary,
  expectedVersion,
  inspectRuntime,
  recordRuntime,
  runtimeMetadataPath
} = require("./difflore-runtime");

function tempDir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function touch(file) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, "fake\n");
}

function realpath(file) {
  return fs.realpathSync(file);
}

function fixture() {
  const root = tempDir("difflore-plugin-root-");
  const pluginRoot = path.join(root, "plugin");
  const pluginData = path.join(root, "data");
  const home = path.join(root, "home");
  const repoRoot = root;
  fs.mkdirSync(pluginRoot, { recursive: true });
  fs.mkdirSync(path.join(repoRoot, "crates"), { recursive: true });
  fs.writeFileSync(path.join(repoRoot, "Cargo.toml"), "[workspace]\n");
  writeJson(path.join(repoRoot, ".codex-plugin", "plugin.json"), {
    name: "difflore",
    version: "0.3.0"
  });
  const executable = new Set();
  const versions = new Map();
  const fx = {
    root,
    repoRoot,
    pluginRoot,
    pluginData,
    home,
    env: {
      PATH: "",
      DIFFLORE_HOME: path.join(home, ".difflore"),
      HOME: home,
      USERPROFILE: home,
      LOCALAPPDATA: path.join(home, "AppData", "Local")
    },
    isExecutable: (candidate) => executable.has(path.resolve(candidate)),
    inspectCommand: (kind, candidate) => {
      const resolved = path.resolve(candidate);
      if (!executable.has(resolved)) {
        return { ok: false, reason: "not marked executable" };
      }
      const version = versions.get(resolved) || "0.3.0";
      return kind === KIND_CLI
        ? { ok: true, version, output: `difflore ${version}` }
        : { ok: true };
    },
    markExecutable(file, version) {
      touch(file);
      executable.add(path.resolve(file));
      if (version) versions.set(path.resolve(file), version);
      return file;
    }
  };
  return fx;
}

function binPair(dir) {
  return {
    cli: path.join(dir, binaryName(KIND_CLI)),
    hook: path.join(dir, binaryName(KIND_HOOK))
  };
}

test("expectedVersion reads the Codex plugin manifest next to plugin/", () => {
  const fx = fixture();
  assert.equal(expectedVersion(fx), "0.3.0");
});

test("install record resolves difflore and adjacent difflore-hook without PATH", () => {
  const fx = fixture();
  const pair = binPair(path.join(fx.root, "installed-bin"));
  fx.markExecutable(pair.cli);
  fx.markExecutable(pair.hook);
  writeJson(path.join(fx.env.DIFFLORE_HOME, "mcp.json"), {
    command: pair.cli,
    args: ["mcp-server"],
    installed_targets: ["Codex"]
  });

  const status = inspectRuntime(fx);

  assert.equal(status.selected.difflore.path, path.resolve(pair.cli));
  assert.equal(status.selected.difflore.source, "install-record");
  assert.equal(status.selected.diffloreHook.path, path.resolve(pair.hook));
  assert.equal(status.selected.diffloreHook.source, "install-record-adjacent");
});

test("managed install dir is searched before PATH", () => {
  const fx = fixture();
  const pair = binPair(path.join(fx.env.DIFFLORE_HOME, "bin"));
  fx.markExecutable(pair.cli);
  fx.markExecutable(pair.hook);

  const status = inspectRuntime(fx);

  assert.equal(status.selected.difflore.path, path.resolve(pair.cli));
  assert.equal(status.selected.difflore.source, "common-install");
  assert.equal(status.selected.diffloreHook.path, path.resolve(pair.hook));
  assert.equal(status.selected.diffloreHook.source, "common-install");
});

test("metadata takes precedence over an older install record", () => {
  const fx = fixture();
  const metadataPair = binPair(path.join(fx.root, "metadata-bin"));
  const recordPair = binPair(path.join(fx.root, "record-bin"));
  for (const file of [metadataPair.cli, metadataPair.hook, recordPair.cli, recordPair.hook]) {
    fx.markExecutable(file);
  }
  writeJson(runtimeMetadataPath(fx), {
    binaries: {
      difflore: metadataPair.cli,
      diffloreHook: metadataPair.hook
    }
  });
  writeJson(path.join(fx.env.DIFFLORE_HOME, "mcp.json"), {
    command: recordPair.cli,
    args: ["mcp-server"]
  });

  const status = inspectRuntime(fx);

  assert.equal(status.selected.difflore.source, "metadata");
  assert.equal(status.selected.difflore.path, path.resolve(metadataPair.cli));
  assert.equal(status.selected.diffloreHook.source, "metadata");
  assert.equal(status.selected.diffloreHook.path, path.resolve(metadataPair.hook));
});

test("recordRuntime writes paths only and does not copy binaries", () => {
  const fx = fixture();
  const pair = binPair(path.join(fx.repoRoot, "target", "release"));
  fx.markExecutable(pair.cli);
  fx.markExecutable(pair.hook);

  const metadata = recordRuntime(fx);
  const saved = JSON.parse(fs.readFileSync(runtimeMetadataPath(fx), "utf8"));

  assert.equal(metadata.binaries.difflore, realpath(pair.cli));
  assert.equal(saved.binaries.diffloreHook, realpath(pair.hook));
  assert.equal(saved.sources.difflore, "repo");
});

test("missing CLI error names checked locations and explicit override", () => {
  const fx = fixture();

  assert.throws(
    () => ensureBinary(KIND_CLI, fx),
    /Unable to find an installed difflore CLI.*DIFFLORE_BINARY.*difflore agents install/s
  );
});

test("CLI version mismatch is rejected unless explicitly allowed", () => {
  const fx = fixture();
  const pair = binPair(path.join(fx.repoRoot, "target", "release"));
  fx.markExecutable(pair.cli, "0.1.0");
  fx.markExecutable(pair.hook);

  const rejected = inspectRuntime(fx);
  assert.equal(rejected.selected.difflore, undefined);
  assert.match(rejected.candidates.difflore.find((c) => c.source === "repo").reason, /expected 0\.3\.0/);

  const allowed = inspectRuntime({
    ...fx,
    env: { ...fx.env, DIFFLORE_ALLOW_VERSION_MISMATCH: "1" }
  });
  assert.equal(allowed.selected.difflore.path, path.resolve(pair.cli));
});

test("packaged MCP and hooks call the plugin runtime wrappers", () => {
  const pluginRoot = path.join(__dirname, "..");
  const mcp = JSON.parse(fs.readFileSync(path.join(pluginRoot, ".mcp.json"), "utf8"));
  const hooks = JSON.parse(fs.readFileSync(path.join(pluginRoot, "hooks", "hooks.json"), "utf8"));

  assert.equal(mcp.mcpServers.difflore.command, "node");
  assert.deepEqual(mcp.mcpServers.difflore.args, ["${PLUGIN_ROOT}/scripts/difflore-mcp.js"]);

  for (const groups of Object.values(hooks.hooks)) {
    for (const group of groups) {
      for (const hook of group.hooks) {
        assert.equal(hook.command, "node \"${PLUGIN_ROOT}/scripts/difflore-hook.js\"");
        assert.doesNotMatch(hook.command, /PATH=/);
        assert.doesNotMatch(hook.command, /command -v difflore-hook/);
      }
    }
  }
});

test("hook wrapper returns Codex noop when runtime is missing", () => {
  const fx = fixture();
  const result = spawnSync(process.execPath, [path.join(__dirname, "difflore-hook.js")], {
    encoding: "utf8",
    input: "{\"hook_event_name\":\"SessionStart\"}",
    env: {
      ...process.env,
      PATH: "",
      PLUGIN_ROOT: fx.pluginRoot,
      DIFFLORE_HOME: fx.env.DIFFLORE_HOME,
      DIFFLORE_PLUGIN_DATA: fx.pluginData,
      HOME: fx.home,
      USERPROFILE: fx.home,
      LOCALAPPDATA: fx.env.LOCALAPPDATA
    }
  });

  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), NOOP_OUTPUT);
  assert.match(result.stderr, /Unable to find an installed difflore-hook hook shim/);
});

test("MCP wrapper self-test fails loudly when runtime is missing", () => {
  const fx = fixture();
  const result = spawnSync(process.execPath, [path.join(__dirname, "difflore-mcp.js"), "--self-test"], {
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: "",
      PLUGIN_ROOT: fx.pluginRoot,
      DIFFLORE_HOME: fx.env.DIFFLORE_HOME,
      DIFFLORE_PLUGIN_DATA: fx.pluginData,
      HOME: fx.home,
      USERPROFILE: fx.home,
      LOCALAPPDATA: fx.env.LOCALAPPDATA
    }
  });

  assert.equal(result.status, 1);
  assert.match(result.stderr, /Unable to find an installed difflore CLI/);
});
