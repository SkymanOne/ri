// Node built-in modules for extensions: the subset pi extensions use, over
// the host's filesystem calls and operations. Each module is a plain object in
// `__yapi_builtins`, so CommonJS `require` can return it synchronously; ES
// imports get a generated wrapper that re-exports its keys.
(() => {
	"use strict";
	const yapi = globalThis.__yapi;
	const native = globalThis.__yapi_native;
	const builtins = {};
	globalThis.__yapi_builtins = builtins;

	const notSupported = (name) => () => {
		const error = new Error(`${name} is not supported in yapi extensions`);
		error.code = "ERR_NOT_SUPPORTED";
		throw error;
	};

	// ----- path (POSIX) ---------------------------------------------------------
	function normalizeString(path, allowAboveRoot) {
		const out = [];
		for (const segment of path.split("/")) {
			if (segment === "" || segment === ".") continue;
			if (segment === "..") {
				if (out.length > 0 && out[out.length - 1] !== "..") out.pop();
				else if (allowAboveRoot) out.push("..");
			} else out.push(segment);
		}
		return out.join("/");
	}
	const path = {
		sep: "/",
		delimiter: ":",
		isAbsolute: (p) => String(p).startsWith("/"),
		normalize(p) {
			p = String(p);
			if (p === "") return ".";
			const absolute = p.startsWith("/");
			const trailing = p.endsWith("/");
			let out = normalizeString(p, !absolute);
			if (out === "" && !absolute) out = ".";
			if (out !== "" && trailing) out += "/";
			return absolute ? `/${out}` : out;
		},
		join(...parts) {
			const joined = parts.filter((part) => {
				if (typeof part !== "string") throw new TypeError("The \"path\" argument must be of type string");
				return part !== "";
			});
			return joined.length === 0 ? "." : path.normalize(joined.join("/"));
		},
		resolve(...parts) {
			let resolved = "";
			let absolute = false;
			for (let i = parts.length - 1; i >= -1 && !absolute; i--) {
				const part = i >= 0 ? String(parts[i]) : process.cwd();
				if (part === "") continue;
				resolved = `${part}/${resolved}`;
				absolute = part.startsWith("/");
			}
			resolved = normalizeString(resolved, !absolute);
			return absolute ? `/${resolved}` : resolved || ".";
		},
		dirname(p) {
			p = String(p);
			if (p === "") return ".";
			const trimmed = p.length > 1 ? p.replace(/\/+$/, "") : p;
			const index = trimmed.lastIndexOf("/");
			if (index === -1) return ".";
			if (index === 0) return "/";
			return trimmed.slice(0, index);
		},
		basename(p, ext) {
			p = String(p);
			const trimmed = p.length > 1 ? p.replace(/\/+$/, "") : p;
			let base = trimmed.slice(trimmed.lastIndexOf("/") + 1);
			if (ext && base.endsWith(ext) && base !== ext) base = base.slice(0, -ext.length);
			return base;
		},
		extname(p) {
			const base = path.basename(p);
			const index = base.lastIndexOf(".");
			return index <= 0 ? "" : base.slice(index);
		},
		relative(from, to) {
			from = path.resolve(from);
			to = path.resolve(to);
			if (from === to) return "";
			const a = from.split("/").filter(Boolean);
			const b = to.split("/").filter(Boolean);
			let common = 0;
			while (common < a.length && common < b.length && a[common] === b[common]) common++;
			return [...Array(a.length - common).fill(".."), ...b.slice(common)].join("/");
		},
		parse(p) {
			const root = String(p).startsWith("/") ? "/" : "";
			const base = path.basename(p);
			const ext = path.extname(p);
			const dir = path.dirname(p);
			return { root, dir: dir === "." && !String(p).includes("/") ? "" : dir, base, ext, name: ext ? base.slice(0, -ext.length) : base };
		},
		format(object) {
			const dir = object.dir || object.root || "";
			const base = object.base || `${object.name ?? ""}${object.ext ?? ""}`;
			return dir ? (dir.endsWith("/") ? `${dir}${base}` : `${dir}/${base}`) : base;
		},
		toNamespacedPath: (p) => p,
		matchesGlob: () => false,
	};
	path.posix = path;
	path.win32 = path;
	path.default = path;
	builtins.path = path;
	builtins["path/posix"] = path;

	// ----- fs ---------------------------------------------------------------------------
	function call(op, args) {
		const result = JSON.parse(native.fs(op, JSON.stringify(args)));
		if (result && result.error) {
			const error = new Error(result.error.message);
			error.code = result.error.code;
			error.syscall = result.error.syscall;
			error.path = result.error.path;
			error.errno = -1;
			throw error;
		}
		return result;
	}
	const toPath = (p) => (p instanceof URL ? builtins.url.fileURLToPath(p) : Buffer.isBuffer(p) ? p.toString() : String(p));
	const absolute = (p) => path.resolve(toPath(p));
	const encodingOf = (options) => (typeof options === "string" ? options : options?.encoding ?? null);
	class Stats {
		constructor(raw) {
			this.size = raw.size;
			this.mode = raw.mode;
			this.mtimeMs = raw.mtimeMs;
			this.atimeMs = raw.atimeMs;
			this.ctimeMs = raw.ctimeMs;
			this.birthtimeMs = raw.birthtimeMs;
			this.mtime = new Date(raw.mtimeMs);
			this.atime = new Date(raw.atimeMs);
			this.ctime = new Date(raw.ctimeMs);
			this.birthtime = new Date(raw.birthtimeMs);
			this.uid = 0;
			this.gid = 0;
			this.nlink = 1;
			this.dev = 0;
			this.ino = 0;
			this.blksize = 4096;
			this.blocks = Math.ceil(raw.size / 512);
			this._type = raw.type;
		}
		isFile() {
			return this._type === "file";
		}
		isDirectory() {
			return this._type === "dir";
		}
		isSymbolicLink() {
			return this._type === "symlink";
		}
		isFIFO() {
			return false;
		}
		isSocket() {
			return false;
		}
		isBlockDevice() {
			return false;
		}
		isCharacterDevice() {
			return false;
		}
	}
	class Dirent {
		constructor(name, type, parent) {
			this.name = name;
			this._type = type;
			this.parentPath = parent;
			this.path = parent;
		}
		isFile() {
			return this._type === "file";
		}
		isDirectory() {
			return this._type === "dir";
		}
		isSymbolicLink() {
			return this._type === "symlink";
		}
		isFIFO() {
			return false;
		}
		isSocket() {
			return false;
		}
		isBlockDevice() {
			return false;
		}
		isCharacterDevice() {
			return false;
		}
	}
	const decodeFile = (result, encoding) => {
		if (result.text !== undefined) return result.text;
		const bytes = Buffer.from(yapi.base64Decode(result.base64));
		return encoding ? bytes.toString(encoding) : bytes;
	};
	const writeArgs = (p, data, options, append) => {
		const args = { path: absolute(p), append };
		if (typeof data === "string") {
			const encoding = encodingOf(options);
			if (encoding && !/^utf-?8$/i.test(encoding)) args.base64 = yapi.base64Encode(Buffer.from(data, encoding));
			else args.text = data;
		} else if (ArrayBuffer.isView(data)) args.base64 = yapi.base64Encode(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
		else args.text = String(data);
		return args;
	};
	const readdirSync = (p, options) => {
		const dir = absolute(p);
		const entries = call("readdir", { path: dir });
		if (options?.recursive) {
			const out = [];
			const walk = (base, prefix) => {
				for (const entry of call("readdir", { path: base })) {
					const name = prefix ? `${prefix}/${entry.name}` : entry.name;
					out.push(options.withFileTypes ? new Dirent(entry.name, entry.type, base) : name);
					if (entry.type === "dir") walk(`${base}/${entry.name}`, name);
				}
			};
			walk(dir, "");
			return out;
		}
		return options?.withFileTypes ? entries.map((entry) => new Dirent(entry.name, entry.type, dir)) : entries.map((entry) => entry.name);
	};
	// ----- file descriptors -------------------------------------------------------------
	// The host works on paths, so a descriptor is an open path and a position,
	// and each read or write goes through the path.
	const descriptors = new Map();
	let nextDescriptor = 3;
	const errno = (code, number, message, syscall, path) => {
		const error = new Error(`${code}: ${message}, ${syscall}${path === undefined ? "" : ` '${path}'`}`);
		Object.assign(error, { code, errno: number, syscall }, path === undefined ? {} : { path });
		return error;
	};
	const descriptor = (fd, syscall) => {
		const entry = descriptors.get(fd);
		if (!entry) throw errno("EBADF", -9, "bad file descriptor", syscall);
		return entry;
	};
	function openSync(p, flags = "r") {
		const target = absolute(p);
		const mode =
			typeof flags === "number"
				? { create: (flags & 64) !== 0, exclusive: (flags & 128) !== 0, truncate: (flags & 512) !== 0, append: (flags & 1024) !== 0 }
				: { create: /[wa]/.test(flags), exclusive: flags.includes("x"), truncate: flags.includes("w"), append: flags.includes("a") };
		const exists = fsSync.existsSync(target);
		if (exists && mode.exclusive) throw errno("EEXIST", -17, "file already exists", "open", target);
		if (!exists && !mode.create) call("stat", { path: target });
		if (!exists || mode.truncate) call("writeFile", { path: target, text: "", append: !mode.truncate });
		const fd = nextDescriptor++;
		descriptors.set(fd, { path: target, position: 0, append: mode.append });
		return fd;
	}
	// A position that is not a number from 0 up means the current one.
	const explicit = (position) => (typeof position === "number" && position >= 0) || typeof position === "bigint";
	function readSync(fd, buffer, offset, length, position) {
		if (offset !== null && typeof offset === "object") ({ offset = 0, length = buffer.byteLength - offset, position = null } = offset);
		offset ??= 0;
		length ??= buffer.byteLength - offset;
		const entry = descriptor(fd, "read");
		const start = explicit(position) ? Number(position) : entry.position;
		const chunk = fsSync.readFileSync(entry.path).subarray(start, start + length);
		new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength).set(chunk, offset);
		if (!explicit(position)) entry.position += chunk.length;
		return chunk.length;
	}
	function writeSync(fd, data, a, b, c) {
		if (fd === 1 || fd === 2) {
			const text = typeof data === "string" ? data : Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString();
			process[fd === 1 ? "stdout" : "stderr"].write(text);
			return Buffer.byteLength(text);
		}
		const entry = descriptor(fd, "write");
		let bytes;
		let position;
		if (typeof data === "string") {
			bytes = Buffer.from(data, typeof b === "string" ? b : "utf8");
			position = a;
		} else {
			const offset = a ?? 0;
			bytes = Buffer.from(data.buffer, data.byteOffset + offset, b ?? data.byteLength - offset);
			position = c;
		}
		const size = fsSync.statSync(entry.path).size;
		const at = entry.append ? size : explicit(position) ? Number(position) : entry.position;
		if (at === size) fsSync.appendFileSync(entry.path, bytes);
		else {
			const next = Buffer.alloc(Math.max(size, at + bytes.length));
			next.set(fsSync.readFileSync(entry.path));
			next.set(bytes, at);
			fsSync.writeFileSync(entry.path, next);
		}
		if (!explicit(position)) entry.position = at + bytes.length;
		return bytes.length;
	}
	function closeSync(fd) {
		descriptor(fd, "close");
		descriptors.delete(fd);
	}
	function truncateSync(p, length = 0) {
		const target = typeof p === "number" ? descriptor(p, "ftruncate").path : absolute(p);
		const current = fsSync.readFileSync(target);
		const next = Buffer.alloc(length);
		next.set(current.subarray(0, length));
		fsSync.writeFileSync(target, next);
	}
	class FileHandle {
		constructor(fd) {
			this.fd = fd;
		}
		async read(buffer, offset, length, position) {
			if (buffer === undefined || !ArrayBuffer.isView(buffer)) {
				const options = buffer ?? {};
				buffer = options.buffer ?? Buffer.alloc(16384);
				({ offset, length, position } = options);
			}
			return { bytesRead: readSync(this.fd, buffer, offset, length, position), buffer };
		}
		async write(data, ...rest) {
			return { bytesWritten: writeSync(this.fd, data, ...rest), buffer: data };
		}
		async readFile(options) {
			return fsSync.readFileSync(descriptor(this.fd, "read").path, options);
		}
		async writeFile(data, options) {
			const entry = descriptor(this.fd, "write");
			fsSync.writeFileSync(entry.path, data, options);
			entry.position = fsSync.statSync(entry.path).size;
		}
		async appendFile(data, options) {
			fsSync.appendFileSync(descriptor(this.fd, "write").path, data, options);
		}
		async stat() {
			return fsSync.statSync(descriptor(this.fd, "fstat").path);
		}
		async truncate(length) {
			truncateSync(this.fd, length);
		}
		async sync() {}
		async datasync() {}
		async close() {
			if (descriptors.has(this.fd)) closeSync(this.fd);
		}
	}
	if (Symbol.asyncDispose) FileHandle.prototype[Symbol.asyncDispose] = FileHandle.prototype.close;
	// Streams over a descriptor. Their classes extend the stream module's,
	// which is defined further down, so they are made on first use.
	let WriteStream;
	function createWriteStream(p, options) {
		const { Writable } = builtins.stream;
		WriteStream ??= class extends Writable {
			constructor(file, settings) {
				super();
				settings = typeof settings === "string" ? { encoding: settings } : (settings ?? {});
				this.path = absolute(file);
				this.bytesWritten = 0;
				this.pending = false;
				this.fd = settings.fd ?? openSync(this.path, settings.flags ?? "w");
				this.defaultEncoding = settings.encoding ?? "utf8";
				queueMicrotask(() => {
					this.emit("open", this.fd);
					this.emit("ready");
				});
			}
			write(chunk, encoding, callback) {
				if (typeof encoding === "function") [callback, encoding] = [encoding, undefined];
				try {
					this.bytesWritten += writeSync(this.fd, typeof chunk === "string" ? Buffer.from(chunk, encoding ?? this.defaultEncoding) : chunk);
				} catch (error) {
					queueMicrotask(() => {
						callback?.(error);
						this.emit("error", error);
					});
					return false;
				}
				if (callback) queueMicrotask(() => callback(null));
				return true;
			}
			end(chunk, encoding, callback) {
				if (typeof chunk === "function") [callback, chunk] = [chunk, undefined];
				else if (typeof encoding === "function") [callback, encoding] = [encoding, undefined];
				if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
				if (callback) this.once("finish", callback);
				queueMicrotask(() => {
					this.close();
					this.emit("finish");
					this.emit("close");
				});
				return this;
			}
			close(callback) {
				if (descriptors.has(this.fd)) closeSync(this.fd);
				if (callback) queueMicrotask(() => callback(null));
			}
			destroy() {
				this.close();
				return this;
			}
		};
		return new WriteStream(p, options);
	}
	let ReadStream;
	function createReadStream(p, options) {
		const { Readable } = builtins.stream;
		ReadStream ??= class extends Readable {
			constructor(file, settings) {
				super();
				settings = typeof settings === "string" ? { encoding: settings } : (settings ?? {});
				this.path = absolute(file);
				this.bytesRead = 0;
				this.pending = false;
				this.encoding = settings.encoding ?? null;
				this.range = [settings.start ?? 0, settings.end === undefined ? undefined : settings.end + 1];
				this.started = false;
				queueMicrotask(() => {
					this.emit("open");
					this.emit("ready");
				});
			}
			setEncoding(encoding) {
				this.encoding = encoding;
				return this;
			}
			/** The whole requested range, read once. */
			content() {
				const bytes = fsSync.readFileSync(this.path).subarray(...this.range);
				this.bytesRead = bytes.length;
				return this.encoding ? bytes.toString(this.encoding) : bytes;
			}
			start() {
				if (this.started) return;
				this.started = true;
				queueMicrotask(() => {
					let content;
					try {
						content = this.content();
					} catch (error) {
						this.emit("error", error);
						return;
					}
					if (content.length > 0) this.emit("data", content);
					this.emit("end");
					this.emit("close");
				});
			}
			on(event, listener) {
				super.on(event, listener);
				if (event === "data") this.start();
				return this;
			}
			pipe(destination) {
				this.on("data", (chunk) => destination.write(chunk));
				this.once("end", () => destination.end?.());
				return destination;
			}
			async *[Symbol.asyncIterator]() {
				this.started = true;
				const content = this.content();
				if (content.length > 0) yield content;
			}
		};
		return new ReadStream(p, options);
	}
	const fsSync = {
		existsSync: (p) => {
			try {
				return call("exists", { path: absolute(p) });
			} catch {
				return false;
			}
		},
		readFileSync: (p, options) => {
			const encoding = encodingOf(options);
			const utf8 = encoding && /^utf-?8$/i.test(encoding);
			return decodeFile(call("readFile", { path: absolute(p), encoding: utf8 ? "utf8" : null }), utf8 ? null : encoding);
		},
		writeFileSync: (p, data, options) => {
			call("writeFile", writeArgs(p, data, options, typeof options === "object" && options?.flag === "a"));
		},
		appendFileSync: (p, data, options) => {
			call("writeFile", writeArgs(p, data, options, true));
		},
		statSync: (p, options) => {
			try {
				return new Stats(call("stat", { path: absolute(p) }));
			} catch (error) {
				if (options?.throwIfNoEntry === false && error.code === "ENOENT") return undefined;
				throw error;
			}
		},
		lstatSync: (p, options) => {
			try {
				return new Stats(call("lstat", { path: absolute(p) }));
			} catch (error) {
				if (options?.throwIfNoEntry === false && error.code === "ENOENT") return undefined;
				throw error;
			}
		},
		readdirSync,
		mkdirSync: (p, options) => {
			const recursive = typeof options === "object" && !!options?.recursive;
			const target = absolute(p);
			const existed = recursive && fsSync.existsSync(target);
			call("mkdir", { path: target, recursive });
			return recursive && !existed ? target : undefined;
		},
		rmSync: (p, options) => {
			call("rm", { path: absolute(p), recursive: !!options?.recursive, force: !!options?.force });
		},
		rmdirSync: (p, options) => {
			call(options?.recursive ? "rm" : "rmdir", { path: absolute(p), recursive: !!options?.recursive });
		},
		unlinkSync: (p) => {
			call("unlink", { path: absolute(p) });
		},
		renameSync: (from, to) => {
			call("rename", { path: absolute(from), to: absolute(to) });
		},
		copyFileSync: (from, to) => {
			call("copyFile", { path: absolute(from), to: absolute(to) });
		},
		cpSync: (from, to, options) => {
			const source = absolute(from);
			const target = absolute(to);
			if (fsSync.statSync(source).isDirectory()) {
				if (!options?.recursive) throw new Error(`Recursive option is required to copy a directory: ${source}`);
				fsSync.mkdirSync(target, { recursive: true });
				for (const name of readdirSync(source)) fsSync.cpSync(`${source}/${name}`, `${target}/${name}`, options);
			} else fsSync.copyFileSync(source, target);
		},
		realpathSync: Object.assign((p) => call("realpath", { path: absolute(p) }), { native: (p) => call("realpath", { path: absolute(p) }) }),
		readlinkSync: (p) => call("readlink", { path: absolute(p) }),
		symlinkSync: (target, p) => {
			call("symlink", { target: toPath(target), path: absolute(p) });
		},
		accessSync: (p) => {
			call("access", { path: absolute(p) });
		},
		chmodSync() {},
		chownSync() {},
		utimesSync() {},
		fsyncSync() {},
		mkdtempSync: (prefix) => {
			const bytes = yapi.randomBytes(6);
			const target = absolute(`${prefix}${Array.from(bytes, (byte) => "abcdefghijklmnopqrstuvwxyz0123456789"[byte % 36]).join("")}`);
			call("mkdir", { path: target, recursive: false });
			return target;
		},
		openSync,
		closeSync,
		readSync,
		writeSync,
		fstatSync: (fd) => fsSync.statSync(descriptor(fd, "fstat").path),
		truncateSync,
		ftruncateSync: (fd, length) => truncateSync(fd, length),
		fdatasyncSync() {},
		watch: () => ({ close() {}, on() {
			return this;
		} }),
		watchFile() {},
		unwatchFile() {},
		createReadStream,
		createWriteStream,
		constants: { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1, O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2, COPYFILE_EXCL: 1 },
		Stats,
		Dirent,
	};
	const promisified = (fn) => (...args) =>
		new Promise((resolve, reject) => {
			try {
				resolve(fn(...args));
			} catch (error) {
				reject(error);
			}
		});
	const fsPromises = {};
	for (const [name, fn] of Object.entries(fsSync)) {
		if (name.endsWith("Sync") && typeof fn === "function") fsPromises[name.slice(0, -4)] = promisified(fn);
	}
	fsPromises.constants = fsSync.constants;
	fsPromises.open = async (p, flags) => new FileHandle(openSync(p, flags));
	fsPromises.default = fsPromises;
	const fs = { ...fsSync, promises: fsPromises };
	for (const [name, fn] of Object.entries(fsSync)) {
		if (!name.endsWith("Sync") || typeof fn !== "function") continue;
		const base = name.slice(0, -4);
		if (base === "exists") {
			fs.exists = (p, callback) => queueMicrotask(() => callback(fsSync.existsSync(p)));
			continue;
		}
		fs[base] = (...args) => {
			const callback = typeof args[args.length - 1] === "function" ? args.pop() : undefined;
			let result;
			let failure = null;
			try {
				result = fn(...args);
			} catch (error) {
				failure = error;
			}
			if (callback) queueMicrotask(() => callback(failure, result));
		};
	}
	// Node passes reads and writes the byte count and the buffer.
	for (const [name, fn] of [
		["read", readSync],
		["write", writeSync],
	]) {
		fs[name] = (fd, data, ...rest) => {
			const callback = rest.pop();
			let count;
			let failure = null;
			try {
				count = fn(fd, data, ...rest);
			} catch (error) {
				failure = error;
			}
			queueMicrotask(() => callback(failure, count, data));
		};
	}
	fs.default = fs;
	builtins.fs = fs;
	builtins["fs/promises"] = fsPromises;

	// ----- os ---------------------------------------------------------------------------------
	const home = yapi.request("home") ?? "/";
	const tmp = yapi.request("tmpdir") ?? "/tmp";
	const os = {
		EOL: "\n",
		homedir: () => home,
		tmpdir: () => tmp,
		platform: () => process.platform,
		type: () => (process.platform === "darwin" ? "Darwin" : "Linux"),
		arch: () => "x64",
		release: () => "",
		version: () => "",
		machine: () => "x86_64",
		hostname: () => "localhost",
		cpus: () => [],
		availableParallelism: () => 1,
		totalmem: () => 0,
		freemem: () => 0,
		loadavg: () => [0, 0, 0],
		uptime: () => 0,
		networkInterfaces: () => ({}),
		endianness: () => "LE",
		userInfo: () => ({ username: process.env.USER || "user", uid: 0, gid: 0, shell: process.env.SHELL || "/bin/sh", homedir: home }),
		constants: { signals: {}, errno: {} },
		devNull: "/dev/null",
	};
	os.default = os;
	builtins.os = os;

	// ----- url ----------------------------------------------------------------------------------
	const url = {
		URL,
		URLSearchParams,
		fileURLToPath(value) {
			const parsed = typeof value === "string" ? new URL(value) : value;
			if (parsed.protocol !== "file:") throw new TypeError("The URL must be of scheme file");
			return decodeURIComponent(parsed.pathname);
		},
		pathToFileURL(p) {
			return new URL(`file://${encodeURI(path.resolve(p)).replace(/\?/g, "%3F").replace(/#/g, "%23")}`);
		},
		format: (value) => String(value),
		parse: (value) => new URL(value),
	};
	url.default = url;
	builtins.url = url;

	// ----- events ---------------------------------------------------------------------------------
	// Node's EventEmitter and stream classes are plain functions, which old
	// packages also call on an object of their own: `EventEmitter.call(this)`.
	// `callable` makes a class answer both ways.
	const callable = (Class) => {
		const Callable = function (...args) {
			if (new.target) return Reflect.construct(Class, args, new.target);
			Object.assign(this, Reflect.construct(Class, args));
			return undefined;
		};
		for (const key of Reflect.ownKeys(Class)) {
			if (key !== "prototype" && key !== "length") Object.defineProperty(Callable, key, Object.getOwnPropertyDescriptor(Class, key));
		}
		Object.setPrototypeOf(Callable, Object.getPrototypeOf(Class));
		Callable.prototype = Class.prototype;
		Object.defineProperty(Class.prototype, "constructor", { value: Callable, writable: true, configurable: true });
		return Callable;
	};
	class EventEmitter {
		constructor() {
			this._events = new Map();
			this._maxListeners = 10;
		}
		_list(event) {
			if (!this._events) this._events = new Map();
			return this._events.get(event) ?? [];
		}
		on(event, listener) {
			this._events ??= new Map();
			this._events.set(event, [...this._list(event), listener]);
			return this;
		}
		addListener(event, listener) {
			return this.on(event, listener);
		}
		prependListener(event, listener) {
			this._events ??= new Map();
			this._events.set(event, [listener, ...this._list(event)]);
			return this;
		}
		once(event, listener) {
			const wrapped = (...args) => {
				this.off(event, wrapped);
				listener.apply(this, args);
			};
			wrapped.listener = listener;
			return this.on(event, wrapped);
		}
		prependOnceListener(event, listener) {
			return this.once(event, listener);
		}
		off(event, listener) {
			this._events?.set(
				event,
				this._list(event).filter((item) => item !== listener && item.listener !== listener),
			);
			return this;
		}
		removeListener(event, listener) {
			return this.off(event, listener);
		}
		removeAllListeners(event) {
			if (event === undefined) this._events = new Map();
			else this._events?.delete(event);
			return this;
		}
		emit(event, ...args) {
			const list = this._list(event);
			if (event === "error" && list.length === 0) throw args[0] instanceof Error ? args[0] : new Error(String(args[0]));
			for (const listener of [...list]) listener.apply(this, args);
			return list.length > 0;
		}
		listeners(event) {
			return this._list(event).map((item) => item.listener ?? item);
		}
		rawListeners(event) {
			return [...this._list(event)];
		}
		listenerCount(event) {
			return this._list(event).length;
		}
		eventNames() {
			return [...(this._events?.keys() ?? [])];
		}
		setMaxListeners(count) {
			this._maxListeners = count;
			return this;
		}
		getMaxListeners() {
			return this._maxListeners;
		}
	}
	EventEmitter = callable(EventEmitter);
	EventEmitter.EventEmitter = EventEmitter;
	EventEmitter.defaultMaxListeners = 10;
	EventEmitter.once = (emitter, event) => new Promise((resolve) => emitter.once(event, (...args) => resolve(args)));
	EventEmitter.on = notSupported("events.on");
	EventEmitter.setMaxListeners = () => {};
	EventEmitter.default = EventEmitter;
	builtins.events = EventEmitter;
	yapi.EventEmitter = EventEmitter;

	// ----- util -------------------------------------------------------------------------------------
	const util = {
		format: yapi.format,
		formatWithOptions: (_options, ...args) => yapi.format(...args),
		inspect: Object.assign((value) => yapi.formatValue(value), { custom: Symbol.for("nodejs.util.inspect.custom"), defaultOptions: {} }),
		promisify(fn) {
			if (typeof fn[util.promisify.custom] === "function") return fn[util.promisify.custom];
			return (...args) =>
				new Promise((resolve, reject) =>
					fn(...args, (error, ...values) => (error ? reject(error) : resolve(values.length > 1 ? values : values[0]))),
				);
		},
		callbackify: (fn) => (...args) => {
			const callback = args.pop();
			fn(...args).then(
				(value) => callback(null, value),
				(error) => callback(error),
			);
		},
		inherits(child, parent) {
			Object.setPrototypeOf(child.prototype, parent.prototype);
			Object.setPrototypeOf(child, parent);
		},
		deprecate: (fn) => fn,
		debuglog: () => () => {},
		isDeepStrictEqual: (a, b) => JSON.stringify(a) === JSON.stringify(b),
		stripVTControlCharacters: (text) => String(text).replace(/\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*(\x07|\x1b\\)/g, ""),
		styleText: (_style, text) => text,
		types: {
			isPromise: (value) => value instanceof Promise,
			isDate: (value) => value instanceof Date,
			isRegExp: (value) => value instanceof RegExp,
			isUint8Array: (value) => value instanceof Uint8Array,
			isNativeError: (value) => value instanceof Error,
			isAsyncFunction: (value) => typeof value === "function" && value.constructor?.name === "AsyncFunction",
		},
		isArray: Array.isArray,
		TextEncoder,
		TextDecoder,
		parseArgs: notSupported("util.parseArgs"),
	};
	util.promisify.custom = Symbol.for("nodejs.util.promisify.custom");
	util.default = util;
	builtins.util = util;

	// ----- child_process --------------------------------------------------------------------------
	class ChildProcess extends EventEmitter {
		constructor() {
			super();
			this.stdout = new EventEmitter();
			this.stderr = new EventEmitter();
			this.stdin = Object.assign(new EventEmitter(), { write() {
				return true;
			}, end() {} });
			this.stdout.setEncoding = () => this.stdout;
			this.stderr.setEncoding = () => this.stderr;
			this.pid = 0;
			this.exitCode = null;
			this.killed = false;
		}
		kill() {
			this.killed = true;
			return true;
		}
		unref() {
			return this;
		}
		ref() {
			return this;
		}
	}
	const runProcess = (command, args, options = {}) =>
		yapi.op("exec", {
			command,
			args,
			shell: !!options.shell,
			cwd: options.cwd ? toPath(options.cwd) : process.cwd(),
			env: options.env,
			timeout: options.timeout,
			input: typeof options.input === "string" ? options.input : undefined,
		});
	const shell = (command) => ({ command: "/bin/sh", args: ["-c", command] });
	const childProcess = {
		ChildProcess,
		spawn(command, args = [], options = {}) {
			if (!Array.isArray(args)) {
				options = args ?? {};
				args = [];
			}
			const child = new ChildProcess();
			const target = options.shell ? shell([command, ...args].join(" ")) : { command, args };
			runProcess(target.command, target.args, options).then(
				(result) => {
					if (result.stdout) child.stdout.emit("data", Buffer.from(result.stdout));
					if (result.stderr) child.stderr.emit("data", Buffer.from(result.stderr));
					child.stdout.emit("end");
					child.stderr.emit("end");
					child.exitCode = result.code;
					child.emit("exit", result.code, result.signal ?? null);
					child.emit("close", result.code, result.signal ?? null);
				},
				(error) => child.emit("error", error),
			);
			return child;
		},
		exec(command, options, callback) {
			if (typeof options === "function") {
				callback = options;
				options = {};
			}
			const target = shell(command);
			const child = childProcess.spawn(target.command, target.args, options ?? {});
			if (callback) collect(child, callback);
			return child;
		},
		execFile(file, args, options, callback) {
			if (typeof args === "function") {
				callback = args;
				args = [];
				options = {};
			} else if (typeof options === "function") {
				callback = options;
				options = {};
			}
			const child = childProcess.spawn(file, args ?? [], options ?? {});
			if (callback) collect(child, callback);
			return child;
		},
		execSync(command, options = {}) {
			const target = shell(command);
			return syncResult(yapi.request("exec.sync", { ...target, cwd: options.cwd ? toPath(options.cwd) : process.cwd(), env: options.env, input: options.input, timeout: options.timeout }), options, command);
		},
		execFileSync(file, args = [], options = {}) {
			return syncResult(yapi.request("exec.sync", { command: file, args, cwd: options.cwd ? toPath(options.cwd) : process.cwd(), env: options.env, input: options.input, timeout: options.timeout }), options, [file, ...args].join(" "));
		},
		spawnSync(command, args = [], options = {}) {
			const target = options.shell ? shell([command, ...args].join(" ")) : { command, args };
			const result = yapi.request("exec.sync", { ...target, cwd: options.cwd ? toPath(options.cwd) : process.cwd(), env: options.env, input: options.input, timeout: options.timeout });
			const encode = (text) => (options.encoding && options.encoding !== "buffer" ? text : Buffer.from(text));
			return { pid: 0, status: result.code, signal: result.signal ?? null, stdout: encode(result.stdout), stderr: encode(result.stderr), output: [null, encode(result.stdout), encode(result.stderr)], error: result.error ? new Error(result.error) : undefined };
		},
		fork: notSupported("child_process.fork"),
	};
	function collect(child, callback) {
		let stdout = "";
		let stderr = "";
		child.stdout.on("data", (chunk) => {
			stdout += chunk.toString();
		});
		child.stderr.on("data", (chunk) => {
			stderr += chunk.toString();
		});
		child.on("error", (error) => callback(error, stdout, stderr));
		child.on("close", (code) => {
			if (code === 0) callback(null, stdout, stderr);
			else {
				const error = new Error(`Command failed with exit code ${code}\n${stderr}`);
				error.code = code;
				callback(error, stdout, stderr);
			}
		});
	}
	function syncResult(result, options, command) {
		if (result.error || result.code !== 0) {
			const error = new Error(`Command failed: ${command}\n${result.stderr ?? ""}`);
			error.status = result.code;
			error.stdout = result.stdout;
			error.stderr = result.stderr;
			throw error;
		}
		return options.encoding && options.encoding !== "buffer" ? result.stdout : Buffer.from(result.stdout);
	}
	childProcess.exec[util.promisify.custom] = (command, options) =>
		new Promise((resolve, reject) =>
			childProcess.exec(command, options, (error, stdout, stderr) => (error ? reject(Object.assign(error, { stdout, stderr })) : resolve({ stdout, stderr }))),
		);
	childProcess.execFile[util.promisify.custom] = (file, args, options) =>
		new Promise((resolve, reject) =>
			childProcess.execFile(file, args, options, (error, stdout, stderr) => (error ? reject(Object.assign(error, { stdout, stderr })) : resolve({ stdout, stderr }))),
		);
	childProcess.default = childProcess;
	builtins.child_process = childProcess;

	// ----- crypto ---------------------------------------------------------------------------------------
	const crypto = {
		randomUUID: () => globalThis.crypto.randomUUID(),
		randomBytes: (size) => Buffer.from(yapi.randomBytes(size)),
		randomInt(min, max) {
			if (max === undefined) {
				max = min;
				min = 0;
			}
			const bytes = yapi.randomBytes(4);
			const value = (bytes[0] * 0x1000000 + bytes[1] * 0x10000 + bytes[2] * 0x100 + bytes[3]) / 0x100000000;
			return min + Math.floor(value * (max - min));
		},
		getRandomValues: (array) => globalThis.crypto.getRandomValues(array),
		createHash(algorithm) {
			const chunks = [];
			const hash = {
				update(data, encoding) {
					chunks.push(typeof data === "string" ? Buffer.from(data, encoding) : Buffer.from(data));
					return hash;
				},
				digest(encoding) {
					const data = yapi.base64Encode(Buffer.concat(chunks));
					const digest = Buffer.from(yapi.base64Decode(yapi.request("hash", { algorithm, data })));
					return encoding ? digest.toString(encoding) : digest;
				},
				copy() {
					return crypto.createHash(algorithm).update(Buffer.concat(chunks));
				},
			};
			return hash;
		},
		createHmac(algorithm, key) {
			// RFC 2104 over createHash.
			const blockSize = /^sha(384|512)$/i.test(algorithm) ? 128 : 64;
			let secret = typeof key === "string" ? Buffer.from(key) : Buffer.from(key?.export?.() ?? key);
			if (secret.length > blockSize) secret = crypto.createHash(algorithm).update(secret).digest();
			const padded = Buffer.alloc(blockSize);
			secret.copy(padded);
			const pad = (byte) => Buffer.from(padded.map((value) => value ^ byte));
			const inner = crypto.createHash(algorithm).update(pad(0x36));
			const hmac = {
				update(data, encoding) {
					inner.update(data, encoding);
					return hmac;
				},
				digest(encoding) {
					return crypto.createHash(algorithm).update(pad(0x5c)).update(inner.digest()).digest(encoding);
				},
			};
			return hmac;
		},
		getHashes: () => ["md5", "sha1", "sha224", "sha256", "sha384", "sha512"],
		getCiphers: () => [],
		getCurves: () => [],
		timingSafeEqual: (a, b) => a.length === b.length && Buffer.from(a).equals(Buffer.from(b)),
		webcrypto: globalThis.crypto,
	};
	crypto.default = crypto;
	builtins.crypto = crypto;

	// ----- small modules ------------------------------------------------------------------------------
	builtins.buffer = { Buffer, Blob, File, atob, btoa, default: { Buffer }, constants: { MAX_LENGTH: 2 ** 31 - 1 } };
	builtins.process = process;
	builtins.timers = { setTimeout, setInterval, setImmediate, clearTimeout, clearInterval, clearImmediate };
	builtins["timers/promises"] = {
		setTimeout: (ms, value) => new Promise((resolve) => setTimeout(() => resolve(value), ms)),
		setImmediate: (value) => new Promise((resolve) => setImmediate(() => resolve(value))),
		scheduler: { wait: (ms) => new Promise((resolve) => setTimeout(resolve, ms)) },
	};
	builtins.perf_hooks = { performance: globalThis.performance };
	builtins.tty = { isatty: () => false };
	builtins.string_decoder = {
		StringDecoder: class StringDecoder {
			constructor(encoding = "utf8") {
				this.encoding = encoding;
				this.pending = new Uint8Array();
			}
			write(buffer) {
				const bytes = new Uint8Array(this.pending.length + buffer.length);
				bytes.set(this.pending);
				bytes.set(buffer, this.pending.length);
				let end = bytes.length;
				// Keep an incomplete UTF-8 sequence for the next write.
				for (let back = 1; back <= 3 && back <= bytes.length; back++) {
					const byte = bytes[bytes.length - back];
					if ((byte & 0xc0) === 0x80) continue;
					const need = byte >= 0xf0 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 1;
					if (need > back) end = bytes.length - back;
					break;
				}
				this.pending = bytes.slice(end);
				return Buffer.from(bytes.subarray(0, end)).toString(this.encoding);
			}
			end(buffer) {
				const text = buffer ? this.write(buffer) : "";
				const rest = Buffer.from(this.pending).toString(this.encoding);
				this.pending = new Uint8Array();
				return text + rest;
			}
		},
	};
	builtins.assert = Object.assign(
		(value, message) => {
			if (!value) throw new Error(message ?? "Assertion failed");
		},
		{
			ok(value, message) {
				if (!value) throw new Error(message ?? "Assertion failed");
			},
			equal(a, b, message) {
				if (a != b) throw new Error(message ?? `${a} == ${b}`);
			},
			strictEqual(a, b, message) {
				if (a !== b) throw new Error(message ?? `${a} === ${b}`);
			},
			deepStrictEqual(a, b, message) {
				if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(message ?? "Values are not deeply equal");
			},
			notStrictEqual(a, b, message) {
				if (a === b) throw new Error(message ?? `${a} !== ${b}`);
			},
			throws(fn) {
				try {
					fn();
				} catch {
					return;
				}
				throw new Error("Missing expected exception");
			},
			fail(message) {
				throw new Error(message ?? "Failed");
			},
		},
	);
	builtins["assert/strict"] = builtins.assert;
	class Stream extends EventEmitter {
		pipe(destination) {
			return destination;
		}
	}
	Stream = callable(Stream);
	class Readable extends Stream {
		static from(iterable) {
			const stream = new Readable();
			queueMicrotask(async () => {
				for await (const chunk of iterable) stream.emit("data", chunk);
				stream.emit("end");
			});
			return stream;
		}
		setEncoding() {
			return this;
		}
		resume() {
			return this;
		}
		pause() {
			return this;
		}
		destroy() {
			return this;
		}
	}
	Readable = callable(Readable);
	class Writable extends Stream {
		write() {
			return true;
		}
		end() {
			this.emit("finish");
			return this;
		}
		destroy() {
			return this;
		}
	}
	Writable = callable(Writable);
	class Transform extends Writable {}
	Transform = callable(Transform);
	// As in Node, the module is the legacy `Stream` constructor, which old
	// packages extend with `util.inherits`, carrying the stream classes.
	builtins.stream = Object.assign(Stream, { Stream, Readable, Writable, Transform, PassThrough: Transform, Duplex: Transform, pipeline: notSupported("stream.pipeline"), finished: notSupported("stream.finished") });
	builtins["stream/promises"] = { pipeline: notSupported("stream.pipeline"), finished: notSupported("stream.finished") };
	builtins.readline = {
		createInterface: () => {
			const emitter = new EventEmitter();
			emitter.close = () => emitter.emit("close");
			emitter.question = (_query, callback) => queueMicrotask(() => callback(""));
			emitter.setPrompt = () => {};
			emitter.prompt = () => {};
			emitter[Symbol.asyncIterator] = async function* () {};
			return emitter;
		},
		emitKeypressEvents() {},
		clearLine() {},
		cursorTo() {},
		moveCursor() {},
	};
	builtins["readline/promises"] = { createInterface: () => ({ question: async () => "", close() {} }) };
	builtins.zlib = { gzipSync: notSupported("zlib.gzipSync"), gunzipSync: notSupported("zlib.gunzipSync"), inflateSync: notSupported("zlib.inflateSync"), deflateSync: notSupported("zlib.deflateSync"), createGzip: notSupported("zlib.createGzip"), createGunzip: notSupported("zlib.createGunzip"), constants: {} };
	builtins["stream/web"] = { ...yapi.webStreams };
	builtins["stream/consumers"] = {
		async buffer(stream) {
			const chunks = [];
			for await (const chunk of stream) chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : Buffer.from(chunk));
			return Buffer.concat(chunks);
		},
		async text(stream) {
			return (await builtins["stream/consumers"].buffer(stream)).toString("utf8");
		},
		async json(stream) {
			return JSON.parse(await builtins["stream/consumers"].text(stream));
		},
		async arrayBuffer(stream) {
			const buffer = await builtins["stream/consumers"].buffer(stream);
			return buffer.buffer.slice(buffer.byteOffset, buffer.byteOffset + buffer.byteLength);
		},
	};
	builtins["util/types"] = builtins.util.types;
	builtins["path/win32"] = builtins.path;

	// ----- querystring ------------------------------------------------------------------------
	const qsEscape = (text) => encodeURIComponent(text);
	const qsUnescape = (text) => {
		try {
			return decodeURIComponent(text);
		} catch {
			return unescape(text);
		}
	};
	const qsValue = (value) => (typeof value === "string" ? value : typeof value === "number" && Number.isFinite(value) ? String(value) : typeof value === "bigint" || typeof value === "boolean" ? String(value) : "");
	const querystring = {
		escape: qsEscape,
		unescape: qsUnescape,
		stringify(object, sep = "&", eq = "=", options) {
			const encode = options?.encodeURIComponent ?? qsEscape;
			if (object === null || typeof object !== "object") return "";
			return Object.keys(object)
				.map((key) => {
					const value = object[key];
					const name = encode(qsValue(key)) + eq;
					return Array.isArray(value) ? value.map((item) => name + encode(qsValue(item))).join(sep) : name + encode(qsValue(value));
				})
				.filter((part) => part.length > 0)
				.join(sep);
		},
		parse(text, sep = "&", eq = "=", options) {
			const decode = options?.decodeURIComponent ?? qsUnescape;
			const maxKeys = options?.maxKeys ?? 1000;
			const result = Object.create(null);
			if (typeof text !== "string" || text.length === 0) return result;
			let parts = text.split(sep);
			if (maxKeys > 0) parts = parts.slice(0, maxKeys);
			for (const part of parts) {
				if (part.length === 0) continue;
				const index = part.indexOf(eq);
				const rawKey = index >= 0 ? part.slice(0, index) : part;
				const rawValue = index >= 0 ? part.slice(index + eq.length) : "";
				const key = decode(rawKey.replace(/\+/g, " "));
				const value = decode(rawValue.replace(/\+/g, " "));
				if (!(key in result)) result[key] = value;
				else if (Array.isArray(result[key])) result[key].push(value);
				else result[key] = [result[key], value];
			}
			return result;
		},
	};
	querystring.encode = querystring.stringify;
	querystring.decode = querystring.parse;
	querystring.default = querystring;
	builtins.querystring = querystring;

	builtins.console = globalThis.console;
	builtins.sys = builtins.util;
	builtins.constants = { ...(builtins.os.constants ?? {}), ...(builtins.fs.constants ?? {}) };

	// ----- worker_threads: the main thread only; workers cannot start ---------------------------
	const environmentData = new Map();
	builtins.worker_threads = {
		isMainThread: true,
		isInternalThread: false,
		threadId: 0,
		threadName: "",
		parentPort: null,
		workerData: null,
		resourceLimits: {},
		SHARE_ENV: Symbol("nodejs.worker_threads.SHARE_ENV"),
		MessageChannel,
		MessagePort,
		BroadcastChannel,
		Worker: class Worker {
			constructor() {
				notSupported("worker_threads.Worker")();
			}
		},
		markAsUncloneable() {},
		markAsUntransferable() {},
		isMarkedAsUntransferable: () => false,
		moveMessagePortToContext: (port) => port,
		receiveMessageOnPort: () => undefined,
		getEnvironmentData: (key) => environmentData.get(key),
		setEnvironmentData: (key, value) => (value === undefined ? environmentData.delete(key) : environmentData.set(key, value)),
		postMessageToThread: notSupported("worker_threads.postMessageToThread"),
	};

	// ----- diagnostics_channel: channels without subscribers from outside ---------------------
	const channels = new Map();
	class Channel {
		constructor(name) {
			this.name = name;
			this._subscribers = [];
		}
		get hasSubscribers() {
			return this._subscribers.length > 0;
		}
		subscribe(handler) {
			this._subscribers.push(handler);
		}
		unsubscribe(handler) {
			const index = this._subscribers.indexOf(handler);
			if (index === -1) return false;
			this._subscribers.splice(index, 1);
			return true;
		}
		publish(message) {
			for (const handler of [...this._subscribers]) handler(message, this.name);
		}
		bindStore() {}
		unbindStore() {
			return false;
		}
		runStores(_data, fn, thisArg, ...args) {
			return fn.apply(thisArg, args);
		}
	}
	const channel = (name) => {
		if (!channels.has(name)) channels.set(name, new Channel(name));
		return channels.get(name);
	};
	const tracingChannel = (nameOrChannels) => {
		const pick = (event) => (typeof nameOrChannels === "string" ? channel(`tracing:${nameOrChannels}:${event}`) : nameOrChannels[event]);
		const tracing = { start: pick("start"), end: pick("end"), asyncStart: pick("asyncStart"), asyncEnd: pick("asyncEnd"), error: pick("error") };
		return {
			...tracing,
			get hasSubscribers() {
				return Object.values(tracing).some((item) => item.hasSubscribers);
			},
			subscribe(handlers) {
				for (const [event, handler] of Object.entries(handlers)) tracing[event]?.subscribe(handler);
			},
			unsubscribe(handlers) {
				for (const [event, handler] of Object.entries(handlers)) tracing[event]?.unsubscribe(handler);
				return true;
			},
			traceSync: (fn, _context, thisArg, ...args) => fn.apply(thisArg, args),
			tracePromise: (fn, _context, thisArg, ...args) => fn.apply(thisArg, args),
			traceCallback: (fn, _position, _context, thisArg, ...args) => fn.apply(thisArg, args),
		};
	};
	builtins.diagnostics_channel = {
		channel,
		hasSubscribers: (name) => channel(name).hasSubscribers,
		subscribe: (name, handler) => channel(name).subscribe(handler),
		unsubscribe: (name, handler) => channel(name).unsubscribe(handler),
		tracingChannel,
		Channel,
	};

	// Modules yapi cannot provide: any use throws. 15-node-exports.js gives them
	// Node's export names so imports link.
	for (const name of ["tls", "http2", "dgram", "cluster", "inspector", "vm", "v8", "dns", "dns/promises", "inspector/promises", "repl", "test", "wasi"]) {
		builtins[name] = new Proxy(
			{},
			{
				get(target, key) {
					if (key === "__esModule" || typeof key === "symbol" || key === "then") return undefined;
					return key in target ? target[key] : notSupported(`node:${name}`);
				},
			},
		);
	}
	// ----- net, http, sea, sqlite ------------------------------------------------------
	// The parts that need no sockets: address checks, which SSRF guards set
	// up when they load, and agents passed to clients. Sockets, servers and
	// requests throw, through the names 15-node-exports.js adds.
	const ipv4 = (text) => {
		const parts = String(text).split(".");
		if (parts.length !== 4) return null;
		let value = 0n;
		for (const part of parts) {
			if (!/^\d{1,3}$/.test(part) || Number(part) > 255) return null;
			value = value * 256n + BigInt(part);
		}
		return value;
	};
	const ipv6 = (text) => {
		let rest = String(text).replace(/%.*$/, "");
		const embedded = rest.match(/(\d+\.\d+\.\d+\.\d+)$/);
		if (embedded) {
			const value = ipv4(embedded[1]);
			if (value === null) return null;
			rest = `${rest.slice(0, -embedded[1].length)}${(value >> 16n).toString(16)}:${(value & 0xffffn).toString(16)}`;
		}
		const halves = rest.split("::");
		if (halves.length > 2) return null;
		const groups = (part) => (part === "" ? [] : part.split(":"));
		const head = groups(halves[0]);
		const tail = halves.length === 2 ? groups(halves[1]) : [];
		const missing = 8 - head.length - tail.length;
		if (halves.length === 1 ? missing !== 0 : missing < 1) return null;
		let value = 0n;
		for (const group of [...head, ...Array(halves.length === 2 ? missing : 0).fill("0"), ...tail]) {
			if (!/^[0-9a-f]{1,4}$/i.test(group)) return null;
			value = (value << 16n) + BigInt(Number.parseInt(group, 16));
		}
		return value;
	};
	const isIPv4 = (text) => ipv4(text) !== null;
	const isIPv6 = (text) => ipv6(text) !== null;
	class SocketAddress {
		constructor({ address, port = 0, family = "ipv4", flowlabel = 0 } = {}) {
			this.address = address ?? (family === "ipv6" ? "::" : "127.0.0.1");
			this.port = port;
			this.family = family;
			this.flowlabel = flowlabel;
		}
	}
	class BlockList {
		#rules = [];
		#value(address, family) {
			if (address instanceof SocketAddress) [address, family] = [address.address, address.family];
			const value = family === "ipv6" ? ipv6(address) : ipv4(address);
			if (value === null) throw Object.assign(new TypeError(`Invalid IP address: ${address}`), { code: "ERR_INVALID_ARG_VALUE" });
			return value;
		}
		#add(text, family, start, end) {
			this.#rules.unshift({ text, family, start, end });
		}
		addAddress(address, family = "ipv4") {
			const value = this.#value(address, family);
			this.#add(`Address: ${family === "ipv6" ? "IPv6" : "IPv4"} ${address}`, family, value, value);
		}
		addRange(start, end, family = "ipv4") {
			this.#add(`Range: ${family === "ipv6" ? "IPv6" : "IPv4"} ${start}-${end}`, family, this.#value(start, family), this.#value(end, family));
		}
		addSubnet(network, prefix, family = "ipv4") {
			const bits = family === "ipv6" ? 128n : 32n;
			const host = bits - BigInt(prefix);
			const start = (this.#value(network, family) >> host) << host;
			this.#add(`Subnet: ${family === "ipv6" ? "IPv6" : "IPv4"} ${network}/${prefix}`, family, start, start + (1n << host) - 1n);
		}
		check(address, family = "ipv4") {
			if (address instanceof SocketAddress) family = address.family;
			let value;
			try {
				value = this.#value(address, family);
			} catch {
				return false;
			}
			return this.#rules.some((rule) => rule.family === family && value >= rule.start && value <= rule.end);
		}
		get rules() {
			return this.#rules.map((rule) => rule.text);
		}
	}
	builtins.net = {
		isIP: (text) => (isIPv4(text) ? 4 : isIPv6(text) ? 6 : 0),
		isIPv4,
		isIPv6,
		BlockList,
		SocketAddress,
	};
	class Agent extends EventEmitter {
		constructor(options = {}) {
			super();
			this.options = { ...options };
			this.keepAlive = options.keepAlive ?? false;
			this.maxSockets = options.maxSockets ?? Infinity;
			this.sockets = {};
			this.requests = {};
			this.freeSockets = {};
		}
		destroy() {}
	}
	builtins.http = { Agent, globalAgent: new Agent() };
	builtins.https = { Agent, globalAgent: new Agent() };
	const notInSea = () => {
		const error = new Error("Operation cannot be invoked when not in a single-executable application");
		error.code = "ERR_NOT_IN_SINGLE_EXECUTABLE_APPLICATION";
		throw error;
	};
	builtins.sea = { isSea: () => false, getAsset: notInSea, getRawAsset: notInSea, getAssetAsBlob: notInSea, getAssetKeys: notInSea };
	const noSqlite = () => notSupported("node:sqlite")();
	builtins.sqlite = {
		DatabaseSync: class DatabaseSync {
			constructor() {
				noSqlite();
			}
		},
		StatementSync: class StatementSync {
			constructor() {
				noSqlite();
			}
		},
		constants: {},
		backup: noSqlite,
	};
	builtins.async_hooks = {
		AsyncLocalStorage: class AsyncLocalStorage {
			#store;
			getStore() {
				return this.#store;
			}
			run(store, callback, ...args) {
				const previous = this.#store;
				this.#store = store;
				try {
					return callback(...args);
				} finally {
					this.#store = previous;
				}
			}
			enterWith(store) {
				this.#store = store;
			}
		},
		AsyncResource: class AsyncResource {},
	};
	builtins.module = {
		createRequire: (filename) => globalThis.__yapi_require_for(toPath(filename)),
		builtinModules: Object.keys(builtins),
		isBuiltin: (name) => String(name).replace(/^node:/, "") in builtins,
	};
})();

// The names an ES module wrapper of builtin `name` exports.
globalThis.__yapi.builtinExports = (name) => Object.keys(globalThis.__yapi_builtins[name] ?? {}).filter((key) => key !== "default" && /^[A-Za-z_$][\w$]*$/.test(key));
