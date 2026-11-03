// ego-bridge preamble. Loaded synchronously from the first line of the
// caller's script, after ego's SDK has defined its globals (see
// `runner::preamble_loader`). It makes absolute paths in the script refer to a per-run
// mirror folder that stands in for the caller's filesystem:
//   - `node:fs` / `node:fs/promises` imported by the script translate paths;
//   - ego APIs that take a path (screenshot, setInputFiles, fetch saveAs,
//     FileChooser.setFiles, Download.saveAs) translate them too.
// Paths reported back (errors, realpath, mkdtemp) are translated back, so the
// script only ever sees caller paths.
import { registerHooks } from "node:module";
import nodePath from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = __EGO_BRIDGE_MIRROR_ROOT__;
const KEY = Symbol.for("ego-bridge.fs");

const isMirrored = (p) => p === ROOT || p.startsWith(ROOT + "/");

function toHost(p) {
  if (typeof p === "string") {
    if (!p.startsWith("/") || isMirrored(p)) return p;
    return ROOT + nodePath.posix.normalize(p);
  }
  if (p instanceof URL && p.protocol === "file:") return pathToFileURL(toHost(fileURLToPath(p)));
  return p;
}

function toCaller(p) {
  if (typeof p !== "string") return p;
  if (p === ROOT) return "/";
  return p.startsWith(ROOT + "/") ? p.slice(ROOT.length) : p;
}

function callerError(err) {
  if (err instanceof Error) {
    if (typeof err.path === "string") err.path = toCaller(err.path);
    if (typeof err.dest === "string") err.dest = toCaller(err.dest);
    if (typeof err.message === "string") err.message = err.message.replaceAll(ROOT, "");
  }
  return err;
}

const toHostList = (value) => (Array.isArray(value) ? value.map(toHost) : toHost(value));

// ---- node:fs ---------------------------------------------------------------

const ONE_PATH = new Set([
  "access", "appendFile", "chmod", "chown", "createReadStream", "createWriteStream",
  "exists", "lchmod", "lchown", "lstat", "lutimes", "mkdir", "mkdtemp", "open",
  "openAsBlob", "opendir", "readFile", "readdir", "readlink", "realpath", "rm",
  "rmdir", "stat", "statfs", "truncate", "unlink", "unwatchFile", "utimes", "watch",
  "watchFile", "writeFile",
]);
const TWO_PATHS = new Set(["copyFile", "cp", "link", "rename", "symlink"]);
const RETURNS_PATH = new Set(["mkdir", "mkdtemp", "readlink", "realpath"]);
const NO_CALLBACK = new Set(["watch", "watchFile", "unwatchFile", "createReadStream", "createWriteStream"]);

function wrapFsFunction(name, fn) {
  const sync = name.endsWith("Sync");
  const base = sync ? name.slice(0, -4) : name;
  const count = TWO_PATHS.has(base) ? 2 : ONE_PATH.has(base) ? 1 : 0;
  if (count === 0) return fn;
  const mapResult = RETURNS_PATH.has(base) ? toCaller : (value) => value;
  const wrapped = function (...args) {
    for (let i = 0; i < count && i < args.length; i += 1) args[i] = toHost(args[i]);
    const last = args.length - 1;
    if (!sync && !NO_CALLBACK.has(base) && last >= 0 && typeof args[last] === "function") {
      const callback = args[last];
      args[last] = function (err, ...rest) {
        return callback.call(this, callerError(err), ...rest.map(mapResult));
      };
    }
    let out;
    try {
      out = fn.apply(this, args);
    } catch (err) {
      throw callerError(err);
    }
    if (out && typeof out.then === "function") {
      return out.then(mapResult, (err) => {
        throw callerError(err);
      });
    }
    return mapResult(out);
  };
  for (const key of Object.keys(fn)) {
    wrapped[key] = typeof fn[key] === "function" ? wrapFsFunction(name, fn[key]) : fn[key];
  }
  return wrapped;
}

function wrapFsModule(mod) {
  const out = {};
  for (const name of Object.keys(mod)) {
    const value = mod[name];
    if (name === "promises" && value && typeof value === "object") out[name] = wrapFsModule(value);
    else if (typeof value === "function" && !/^[A-Z]/.test(name)) out[name] = wrapFsFunction(name, value);
    else out[name] = value;
  }
  return out;
}

const wrappedFs = wrapFsModule(process.getBuiltinModule("node:fs"));
globalThis[KEY] = { fs: wrappedFs, promises: wrappedFs.promises };

const FS_SPECIFIERS = new Map([
  ["fs", "fs"],
  ["node:fs", "fs"],
  ["fs/promises", "promises"],
  ["node:fs/promises", "promises"],
]);
const VIRTUAL_PREFIX = "ego-bridge:";
const IDENTIFIER = /^[A-Za-z_$][\w$]*$/;

// The caller's script is evaluated under a URL ending in `/[eval<n>]` or
// `/[stdin<n>]` (ego uses `file:////[eval2]`, plain node `file:///<cwd>/[eval1]`).
// Only its imports are redirected; ego's own SDK keeps the real fs.
const isCallerScript = (parentURL) =>
  typeof parentURL === "string" &&
  parentURL.startsWith("file:") &&
  /\/(\[|%5B)(eval|stdin)\d*(\]|%5D)$/i.test(parentURL);

function moduleSource(kind) {
  const names = Object.keys(globalThis[KEY][kind]).filter((n) => IDENTIFIER.test(n) && n !== "default");
  return [
    `const m = globalThis[Symbol.for("ego-bridge.fs")].${kind};`,
    "export default m;",
    ...names.map((n) => `export const ${n} = m.${n};`),
  ].join("\n");
}

registerHooks({
  resolve(specifier, context, nextResolve) {
    const kind = FS_SPECIFIERS.get(specifier);
    if (kind && isCallerScript(context.parentURL)) {
      return { url: VIRTUAL_PREFIX + kind, shortCircuit: true };
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url.startsWith(VIRTUAL_PREFIX)) {
      return { format: "module", source: moduleSource(url.slice(VIRTUAL_PREFIX.length)), shortCircuit: true };
    }
    return nextLoad(url, context);
  },
});

// ---- ego APIs --------------------------------------------------------------

const patchedPrototypes = new WeakSet();

function patchPrototype(instance, patches) {
  const proto = instance && Object.getPrototypeOf(instance);
  if (!proto || patchedPrototypes.has(proto)) return instance;
  patchedPrototypes.add(proto);
  for (const [name, wrap] of Object.entries(patches)) {
    const original = proto[name];
    if (typeof original === "function") proto[name] = wrap(original);
  }
  return instance;
}

const patchDownload = (download) =>
  patchPrototype(download, {
    saveAs: (orig) => function (path, ...rest) {
      return orig.call(this, toHost(path), ...rest);
    },
  });

const patchChooser = (chooser) =>
  patchPrototype(chooser, {
    setFiles: (orig) => function (paths, ...rest) {
      return orig.call(this, toHostList(paths), ...rest);
    },
  });

const patchPage = (page) =>
  patchPrototype(page, {
    screenshot: (orig) => function (options, ...rest) {
      const mapped = options && typeof options === "object" && "path" in options
        ? { ...options, path: toHost(options.path) }
        : options;
      return orig.call(this, mapped, ...rest);
    },
    setInputFiles: (orig) => function (selector, paths, ...rest) {
      return orig.call(this, selector, toHostList(paths), ...rest);
    },
    fetch: (orig) => function (url, options, ...rest) {
      const mapped = options && typeof options === "object" && "saveAs" in options
        ? { ...options, saveAs: toHost(options.saveAs) }
        : options;
      return orig.call(this, url, mapped, ...rest);
    },
    waitForFileChooser: (orig) => function (...args) {
      return Promise.resolve(orig.apply(this, args)).then(patchChooser);
    },
    waitForEvent: (orig) => function (event, ...rest) {
      const result = Promise.resolve(orig.call(this, event, ...rest));
      return event === "download" ? result.then(patchDownload) : result;
    },
  });

function patchTaskSpace(task) {
  if (task && typeof task.page === "function") patchPage(task.page("p1"));
  return task;
}

for (const name of ["taskSpace", "claimTaskSpace", "takeOverTaskSpace"]) {
  const original = globalThis[name];
  if (typeof original !== "function") continue;
  globalThis[name] = function (...args) {
    return Promise.resolve(original.apply(this, args)).then(patchTaskSpace);
  };
}
