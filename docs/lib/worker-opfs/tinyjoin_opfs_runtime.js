var e = class extends Error {
	code;
	retryable;
	constructor(e, t, n = !1) {
		super(t), this.name = "StorageError", this.code = e, this.retryable = n;
	}
};
function t(t) {
	if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(t)) throw new e("INVALID_STORAGE_NAME", "An OPFS database name must be 1-64 ASCII letters, numbers, dots, underscores, or hyphens, and start with a letter or number");
}
var n = 4096, r = 65536, i = n * r, a = class {
	#e;
	#t = !1;
	constructor(e) {
		this.#e = e;
		try {
			this.#i();
		} catch (t) {
			this.#t = !0;
			try {
				e.close();
			} catch {}
			throw t;
		}
	}
	pageCount() {
		return this.#n(), this.#r();
	}
	readPage(e, t, r) {
		this.#n();
		let i = o(e, t);
		if (l(r, "The read target"), i >= this.#r()) throw RangeError(`Page ${i} has not been allocated`);
		return f(this.#e, d(i), r), n;
	}
	writePage(e, t, r) {
		this.#n();
		let i = s(e, t);
		l(r, "The page source");
		let a = this.#r();
		if (i > a) throw RangeError(`Page ${i} cannot be written before page ${a} is allocated`);
		try {
			i === a ? p(this.#e, d(i), r) : m(this.#e, d(i), r);
		} catch (e) {
			throw i === a && this.#a(a), e;
		}
		return n;
	}
	flush() {
		this.#n();
		try {
			this.#e.flush();
		} catch (e) {
			throw g(e, "STORAGE_WRITE_FAILED", "TinyJoin could not flush database pages");
		}
	}
	close() {
		if (!this.#t) {
			this.#t = !0;
			try {
				this.#e.close();
			} catch (e) {
				throw g(e, "STORAGE_CLOSE_FAILED", "TinyJoin could not close its page device");
			}
		}
	}
	#n() {
		if (this.#t) throw new e("STORAGE_CLOSED", "The TinyJoin page device is closed");
	}
	#r() {
		let t;
		try {
			t = this.#e.getSize();
		} catch (e) {
			throw g(e, "STORAGE_READ_FAILED", "TinyJoin could not read the database page count");
		}
		if (!Number.isSafeInteger(t) || t < 0) throw new e("STORAGE_CORRUPT", "The TinyJoin page file has an invalid byte length");
		if (t > 268435456) throw new e("STORAGE_DATABASE_TOO_LARGE", `The TinyJoin page file exceeds ${i} bytes`);
		if (t % 4096 != 0) throw new e("STORAGE_CORRUPT", "The TinyJoin page file is not aligned to its page size");
		return t / n;
	}
	#i() {
		let t;
		try {
			t = this.#e.getSize();
		} catch (e) {
			throw g(e, "STORAGE_READ_FAILED", "TinyJoin could not read the database page count");
		}
		if (!Number.isSafeInteger(t) || t < 0) throw new e("STORAGE_CORRUPT", "The TinyJoin page file has an invalid byte length");
		if (t > 268435456) throw new e("STORAGE_DATABASE_TOO_LARGE", `The TinyJoin page file exceeds ${i} bytes`);
		let r = t - t % n;
		if (r !== t) try {
			this.#e.truncate(r), this.#e.flush();
		} catch (t) {
			throw new e("STORAGE_COMMIT_OUTCOME_UNKNOWN", g(t, "STORAGE_WRITE_FAILED", "TinyJoin could not repair a torn trailing database page").message);
		}
	}
	#a(t) {
		try {
			this.#e.truncate(t * n), this.#e.flush();
		} catch (t) {
			throw new e("STORAGE_COMMIT_OUTCOME_UNKNOWN", `TinyJoin could not roll back a failed page append${t instanceof Error && t.message ? `: ${t.message}` : ""}`);
		}
	}
};
function o(e, t) {
	if (!c(e) || !c(t)) throw RangeError("Page id words must be unsigned 32-bit integers");
	if (t !== 0 || e >= 65536) throw RangeError("Page id must be between 0 and 65535");
	return e;
}
function s(t, n) {
	if (!c(t) || !c(n)) throw RangeError("Page id words must be unsigned 32-bit integers");
	if (n !== 0 || t > 65536) throw RangeError(`Page id must be between 0 and ${r}`);
	if (t === 65536) throw new e("STORAGE_DATABASE_TOO_LARGE", `The TinyJoin database cannot exceed ${i} bytes`);
	return t;
}
function c(e) {
	return Number.isInteger(e) && e >= 0 && e <= 4294967295;
}
function l(e, t) {
	if (u(e, t), e.byteLength !== 4096) throw RangeError(`${t} must contain exactly ${n} bytes`);
}
function u(e, t) {
	if (!(e instanceof Uint8Array)) throw TypeError(`${t} must be a Uint8Array`);
}
function d(e) {
	return e * n;
}
function f(t, n, r) {
	let i = 0;
	try {
		for (; i < r.byteLength;) {
			let a = t.read(r.subarray(i), { at: n + i });
			if (!Number.isInteger(a) || a <= 0 || a > r.byteLength - i) throw new e("STORAGE_READ_FAILED", "TinyJoin received a short database page read with no progress", !0);
			i += a;
		}
	} catch (e) {
		throw g(e, "STORAGE_READ_FAILED", "TinyJoin could not read a database page");
	}
}
function p(t, n, r) {
	let i = 0;
	try {
		for (; i < r.byteLength;) {
			let a = t.write(r.subarray(i), { at: n + i });
			if (!Number.isInteger(a) || a <= 0 || a > r.byteLength - i) throw new e("STORAGE_WRITE_FAILED", "TinyJoin received a short database page write with no progress", !0);
			i += a;
		}
	} catch (e) {
		throw g(e, "STORAGE_WRITE_FAILED", "TinyJoin could not write a database page");
	}
}
function m(t, n, r) {
	let i = 0;
	for (; i < r.byteLength;) {
		let a;
		try {
			a = t.write(r.subarray(i), { at: n + i });
		} catch (e) {
			throw h(e);
		}
		if (!Number.isInteger(a) || a < 0 || a > r.byteLength - i) throw h(/* @__PURE__ */ Error("The page device returned an invalid write length"));
		if (a === 0) throw i === 0 ? new e("STORAGE_WRITE_FAILED", "TinyJoin received a short database page write with no progress", !0) : h(/* @__PURE__ */ Error("A partial database page write stopped making progress"));
		i += a;
	}
}
function h(t) {
	return new e("STORAGE_COMMIT_OUTCOME_UNKNOWN", `TinyJoin could not safely complete an in-place page write: ${g(t, "STORAGE_WRITE_FAILED", "TinyJoin could not write a database page").message}`);
}
function g(t, n, r) {
	return t instanceof e ? t : typeof t == "object" && t && "code" in t && typeof t.code == "string" && "message" in t && typeof t.message == "string" ? new e(t.code, t.message) : (typeof t == "object" && t && "name" in t ? String(t.name) : "") === "QuotaExceededError" ? new e("STORAGE_QUOTA_EXCEEDED", "The browser has no space available for TinyJoin database pages", !0) : new e(n, t instanceof Error && t.message ? `${r}: ${t.message}` : r, !0);
}
var _ = "database.pages", v = "tinyjoin-pages-v1";
async function y(e, n) {
	t(e);
	let r = n ?? C(), i, o;
	try {
		return i = await x(await (await (await (await r.getDirectory()).getDirectoryHandle(v, { create: !0 })).getDirectoryHandle(`db-${e}`, { create: !0 })).getFileHandle(_, { create: !0 })), o = new a(i), i = void 0, new b(o);
	} catch (e) {
		throw o === void 0 ? i !== void 0 && S(i) : S(o), w(e, "OPFS_UNAVAILABLE", "TinyJoin could not open its OPFS page database");
	}
}
var b = class {
	pageDevice;
	#e = !1;
	constructor(e) {
		this.pageDevice = e;
	}
	close() {
		this.#e || (this.#e = !0, this.pageDevice.close());
	}
};
async function x(t) {
	if (typeof t.createSyncAccessHandle != "function") throw new e("OPFS_UNAVAILABLE", "TinyJoin OPFS page storage requires synchronous access handles in a dedicated Worker");
	try {
		return await t.createSyncAccessHandle();
	} catch (e) {
		throw w(e, "OPFS_UNAVAILABLE", "TinyJoin could not acquire its OPFS page database");
	}
}
function S(e) {
	try {
		e.close();
	} catch {}
}
function C() {
	let t = globalThis.navigator?.storage;
	if (typeof t?.getDirectory != "function") throw new e("OPFS_UNAVAILABLE", "Origin private file system storage is unavailable in this runtime");
	return { getDirectory: () => t.getDirectory() };
}
function w(t, n, r) {
	if (t instanceof e) return t;
	if (typeof t == "object" && t && "code" in t && typeof t.code == "string" && "message" in t && typeof t.message == "string") return new e(t.code, t.message, "retryable" in t && typeof t.retryable == "boolean" && t.retryable);
	let i = T(t), a = t instanceof Error && t.message ? `${r}: ${t.message}` : r;
	return i === "NoModificationAllowedError" ? new e("STORAGE_LOCKED", "Another TinyJoin worker already has this OPFS database open", !0) : i === "QuotaExceededError" ? new e("STORAGE_QUOTA_EXCEEDED", "The browser has no space available for TinyJoin OPFS page storage", !0) : i === "InvalidStateError" || i === "NotAllowedError" || i === "SecurityError" ? new e("OPFS_UNAVAILABLE", a) : new e(n, a, !0);
}
function T(e) {
	return typeof e == "object" && e && "name" in e ? String(e.name) : "";
}
async function E(e, t, n) {
	let r = await (n.createSession ?? y)(e, t);
	try {
		return await n.createPageEngine(r.pageDevice);
	} catch (e) {
		try {
			r.close();
		} catch {}
		throw e;
	}
}
export { E as createOpfsWasmEngine };
