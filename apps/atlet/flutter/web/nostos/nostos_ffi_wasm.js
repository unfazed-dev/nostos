/* @ts-self-types="./nostos_ffi_wasm.d.ts" */

/**
 * A replication frame, mirrored from `nostos_core::Frame` into JS-friendly types.
 *
 * `payload` is an optional `Uint8Array`-backed `Vec<u8>` (the opaque tuple
 * image); `None`/null/undefined for deletes. `lsn` is `f64` (see module docs).
 */
export class Frame {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        FrameFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_frame_free(ptr, 0);
    }
    /**
     * Build a frame from JS. `op` is `"insert" | "update" | "delete"`.
     * `payload` may be null/undefined (deletes); `txn_id` may be null/undefined.
     *
     * `lsn` and `txn_id` are `f64` to avoid BigInt at the JS boundary; they're
     * narrowed to `u64` internally (real LSNs never approach 2^53).
     * @param {number} lsn
     * @param {string} op
     * @param {string} table
     * @param {string} pk
     * @param {Uint8Array | null} [payload]
     * @param {number | null} [txn_id]
     */
    constructor(lsn, op, table, pk, payload, txn_id) {
        const ptr0 = passStringToWasm0(op, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len1 = WASM_VECTOR_LEN;
        const ptr2 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len2 = WASM_VECTOR_LEN;
        var ptr3 = isLikeNone(payload) ? 0 : passArray8ToWasm0(payload, wasm.__wbindgen_export);
        var len3 = WASM_VECTOR_LEN;
        const ret = wasm.frame_new(lsn, ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3, !isLikeNone(txn_id), isLikeNone(txn_id) ? 0 : txn_id);
        this.__wbg_ptr = ret;
        FrameFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
}
if (Symbol.dispose) Frame.prototype[Symbol.dispose] = Frame.prototype.free;

/**
 * The Nostos apply engine, running in-memory in the browser.
 *
 * Construct with `new NostosEngine()`. Feed frames; flush to commit a pending
 * batch; read `checkpoint` to drive `resume_lsn` on reconnect.
 *
 * ## `where_sql` (ADR-0012)
 *
 * The engine carries an optional `where_sql` predicate string
 * ([`NostosEngine::set_where_sql`]) that the WASM transport (E1) will attach to
 * the subscribe frame when it connects. The apply engine itself does NOT
 * evaluate it — the server compiles + ANDs it into the session predicate, so
 * only matching rows are ever sent. Storing it on the engine lets E1 read it
 * at connect time without a separate config object crossing the JS boundary.
 */
export class NostosEngine {
    static __wrap(ptr) {
        const obj = Object.create(NostosEngine.prototype);
        obj.__wbg_ptr = ptr;
        NostosEngineFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        NostosEngineFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_nostosengine_free(ptr, 0);
    }
    /**
     * Validate principal and role before applying a Function page. A role
     * change returns `resnapshot:true`; the Worker must pull again from zero.
     * @param {string} body
     * @returns {string}
     */
    applyAppwritePage(body) {
        let deferred3_0;
        let deferred3_1;
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(body, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_applyAppwritePage(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            var ptr2 = r0;
            var len2 = r1;
            if (r3) {
                ptr2 = 0; len2 = 0;
                throw takeObject(r2);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
            wasm.__wbindgen_export5(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * Materialize the WS2 read-views over `nostos_data`. After this,
     * `SELECT col FROM <table>` resolves against a VIEW that
     * `json_extract`s each column from the opaque payload. SqliteWasm only
     * (Memory is a no-op). Mirrors native `SqliteStorage::apply_schema`.
     *
     * JS: `eng.applySchema([{name, columns}, ...])`
     * @param {any[]} tables
     */
    applySchema(tables) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passArrayJsValueToWasm0(tables, wasm.__wbindgen_export);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_applySchema(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * @param {string} id
     */
    appwriteAck(id) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(id, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_appwriteAck(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * @returns {string}
     */
    get appwriteAfter() {
        let deferred2_0;
        let deferred2_1;
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostosengine_appwriteAfter(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            var ptr1 = r0;
            var len1 = r1;
            if (r3) {
                ptr1 = 0; len1 = 0;
                throw takeObject(r2);
            }
            deferred2_0 = ptr1;
            deferred2_1 = len1;
            return getStringFromWasm0(ptr1, len1);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
            wasm.__wbindgen_export5(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * @returns {string}
     */
    appwritePending() {
        let deferred2_0;
        let deferred2_1;
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostosengine_appwritePending(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            var ptr1 = r0;
            var len1 = r1;
            if (r3) {
                ptr1 = 0; len1 = 0;
                throw takeObject(r2);
            }
            deferred2_0 = ptr1;
            deferred2_1 = len1;
            return getStringFromWasm0(ptr1, len1);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
            wasm.__wbindgen_export5(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * @param {string} id
     * @param {string} error
     * @param {boolean} permanent
     */
    appwriteReject(id, error, permanent) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(id, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(error, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            wasm.nostosengine_appwriteReject(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, permanent);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    appwriteRevoke() {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostosengine_appwriteRevoke(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Bind the locally authenticated user before exposing cached rows. The
     * Function independently verifies the JWT on every cloud request.
     * @param {string} endpoint
     * @param {string} project
     * @param {string} _function
     * @param {string} user
     */
    bindAppwritePrincipal(endpoint, project, _function, user) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(endpoint, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(project, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(_function, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len2 = WASM_VECTOR_LEN;
            const ptr3 = passStringToWasm0(user, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len3 = WASM_VECTOR_LEN;
            wasm.nostosengine_bindAppwritePrincipal(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The current durable checkpoint (the LSN to send as `resume_lsn` on a
     * reconnect). 0 until the first commit.
     * @returns {number}
     */
    get checkpoint() {
        const ret = wasm.nostosengine_checkpoint(this.__wbg_ptr);
        return ret;
    }
    /**
     * ADR-0029 D1: wipe the in-memory rows AND outbox — the sign-out
     * local-state wipe for the browser. The `NostosEngine` has no checkpoint
     * file; this clears the live in-memory store so the next user (same
     * Worker/page session) does not see the previous user's rows. Call before
     * `NostosSocket::close` on sign-out.
     */
    clear() {
        wasm.nostosengine_clear(this.__wbg_ptr);
    }
    /**
     * Decrement the PN-Counter by `delta` (bumps the negative counter `n`).
     * @param {string} table
     * @param {string} pk
     * @param {number} delta
     * @returns {number}
     */
    counterDecrement(table, pk, delta) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            wasm.nostosengine_counterDecrement(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, delta);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Increment the PN-Counter in row `pk` of `table` by `delta` (ADR-0030
     * addendum / ADR-0032 T4). Read-modify-write: reads the current counter
     * payload, applies the delta to this replica's entry, and enqueues the
     * result.
     *
     * ponytail: mirrors SyncClient::counter_op (client.rs L665); rewire to
     * share when convenient.
     * @param {string} table
     * @param {string} pk
     * @param {number} delta
     * @returns {number}
     */
    counterIncrement(table, pk, delta) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            wasm.nostosengine_counterIncrement(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, delta);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The current dead-lettered write count. For `watchWriteStatus`.
     * @returns {number}
     */
    get deadLetteredCount() {
        const ret = wasm.nostosengine_deadLetteredCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Feed a frame. Returns an `Outcome` if the frame triggered a commit (a
     * transaction boundary or the soft cap), or `undefined` if the frame was
     * buffered pending a future boundary. Throws on a backend error (the
     * in-memory backend never errors, but the contract is preserved).
     * @param {Frame} frame
     * @returns {Outcome | undefined}
     */
    feed(frame) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            _assertClass(frame, Frame);
            var ptr0 = frame.__destroy_into_raw();
            wasm.nostosengine_feed(retptr, this.__wbg_ptr, ptr0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            if (r2) {
                throw takeObject(r1);
            }
            return r0 === 0 ? undefined : Outcome.__wrap(r0);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Flush any buffered frames as one atomic commit. Returns `undefined` if
     * nothing was pending. Call this when the stream goes idle or the
     * connection closes to make the last partial batch durable.
     * @returns {Outcome | undefined}
     */
    flush() {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostosengine_flush(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            if (r2) {
                throw takeObject(r1);
            }
            return r0 === 0 ? undefined : Outcome.__wrap(r0);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The last error from the most recent dead-lettered write (or null).
     * @returns {string | undefined}
     */
    get lastError() {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostosengine_lastError(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            let v1;
            if (r0 !== 0) {
                v1 = getStringFromWasm0(r0, r1).slice();
                wasm.__wbindgen_export5(r0, r1 * 1, 1);
            }
            return v1;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Create an in-memory engine. Data survives the apply loop but NOT a page
     * reload — durable browser persistence (SQLite-WASM/OPFS) is the Worker's
     * durable path (ADR-0017 follow-up / ADR-0033). This is the node-smoke +
     * standalone default + the OPFS-unavailable degrade path.
     */
    constructor() {
        const ret = wasm.nostosengine_new();
        this.__wbg_ptr = ret;
        NostosEngineFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * Browser direct-mode engine. The Worker supplies a device ID persisted in
     * the same OPFS database as the outbox, never a per-page-load random ID.
     * @param {object | null | undefined} db
     * @param {string} device_id
     * @returns {NostosEngine}
     */
    static newAppwrite(db, device_id) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(device_id, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_newAppwrite(retptr, isLikeNone(db) ? 0 : addHeapObject(db), ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            if (r2) {
                throw takeObject(r1);
            }
            return NostosEngine.__wrap(r0);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Add `element` to the add-wins OR-set in row `pk` of `table` (ADR-0030 /
     * ADR-0032 T4). Mints a client HLC and enqueues a merge-upsert. The
     * element renders locally immediately and converges with concurrent
     * remote adds on the server's echo.
     *
     * ponytail: mirrors SyncClient::or_set_add (client.rs L571); rewire to
     * share when convenient.
     * @param {string} table
     * @param {string} pk
     * @param {string} element
     * @returns {number}
     */
    orSetAdd(table, pk, element) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(element, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len2 = WASM_VECTOR_LEN;
            wasm.nostosengine_orSetAdd(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Remove `element` from the OR-set — a tombstone at a fresh HLC. Add-wins:
     * a concurrent or later re-add (a higher HLC) re-activates the element.
     * @param {string} table
     * @param {string} pk
     * @param {string} element
     * @returns {number}
     */
    orSetRemove(table, pk, element) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(element, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len2 = WASM_VECTOR_LEN;
            wasm.nostosengine_orSetRemove(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The current pending (non-dead-lettered) write count. For
     * `watchWriteStatus`.
     * @returns {number}
     */
    get pendingCount() {
        const ret = wasm.nostosengine_pendingCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Run an arbitrary SELECT, returning a JSON-array-of-objects string.
     * SqliteWasm only (Memory returns `"[]"`). Mirrors native
     * `SqliteStorage::query` (sqlite.rs L416).
     * @param {string} sql
     * @returns {string}
     */
    query(sql) {
        let deferred3_0;
        let deferred3_1;
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(sql, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_query(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            var ptr2 = r0;
            var len2 = r1;
            if (r3) {
                ptr2 = 0; len2 = 0;
                throw takeObject(r2);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
            wasm.__wbindgen_export5(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * How many rows the in-memory store currently holds.
     * @returns {number}
     */
    get rowCount() {
        const ret = wasm.nostosengine_rowCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Enumerate the `(pk, payload)` pairs the engine currently holds for
     * `table`, sorted by pk. The readback the browser demo renders from: each
     * entry's `payload` is a `Uint8Array` (the opaque tuple image the engine
     * applied); decode/interpret on the JS side.
     *
     * This is a JS/diagnostics convenience — NOT part of the `Storage` trait
     * (the trait stays minimal: `checkpoint` + `apply_batch`). It reaches the
     * concrete `InMemoryStorage` through the engine's read-only accessor.
     * Deletes are excluded (a delete removes the row from the store, so its pk
     * is absent); the enumeration reflects the engine's *current* state, not
     * its event history.
     *
     * JS:
     * ```js
     * for (const entry of eng.rowsFor("tasks")) {
     *   console.log(entry.pk, entry.payload);  // string, Uint8Array
     * }
     * ```
     * @param {string} table
     * @returns {RowEntry[]}
     */
    rowsFor(table) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_rowsFor(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var v2 = getArrayJsValueFromWasm0(r0, r1).slice();
            wasm.__wbindgen_export5(r0, r1 * 4, 4);
            return v2;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Configure which tables are OR-set / counter CRDTs. Call BEFORE any
     * orSet/counter verb — the loud-fail gate checks the tag before minting.
     * Mirrors `SyncClientConfig::or_set_tables` / `counter_tables`.
     * @param {string[]} or_set
     * @param {string[]} counter
     */
    setCrdtTables(or_set, counter) {
        const ptr0 = passArrayJsValueToWasm0(or_set, wasm.__wbindgen_export);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArrayJsValueToWasm0(counter, wasm.__wbindgen_export);
        const len1 = WASM_VECTOR_LEN;
        wasm.nostosengine_setCrdtTables(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    }
    /**
     * Set the `where_sql` predicate the transport (E1) will attach to the next
     * subscribe frame — e.g. `"priority > 5"`. Pass `null`/`undefined` to clear
     * it. The grammar is the safe-SQL subset (six comparison operators +
     * `AND`/`OR`/`NOT` + parens); a parse failure closes the server socket with
     * an `invalid where_sql:` reason before any event flows. The apply engine
     * stores this for E1; it does not evaluate it locally (the server filters).
     *
     * JS:
     * ```js
     * const eng = new NostosEngine();
     * eng.setWhereSql("status = open AND priority >= 3");
     * ```
     * @param {string | null} [sql]
     */
    setWhereSql(sql) {
        var ptr0 = isLikeNone(sql) ? 0 : passStringToWasm0(sql, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len0 = WASM_VECTOR_LEN;
        wasm.nostosengine_setWhereSql(this.__wbg_ptr, ptr0, len0);
    }
    /**
     * The configured `where_sql`, or `null` if none. E1's transport reads this
     * when building the subscribe frame.
     * @returns {string | undefined}
     */
    get whereSql() {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostosengine_whereSql(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            let v1;
            if (r0 !== 0) {
                v1 = getStringFromWasm0(r0, r1).slice();
                wasm.__wbindgen_export5(r0, r1 * 1, 1);
            }
            return v1;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Enqueue a batch of writes atomically (ADR-0032 T3). All ops commit in
     * one SQLite txn (SqliteWasm) or one BTreeMap extend (Memory) — a
     * mid-batch failure rolls back the entire batch. Returns the outbox ids
     * in order. Each op is also `apply_local`'d for instant optimistic UI.
     *
     * JS: `eng.writeBatch([{table, op, pk, payloadJson?}, ...])` → `[id1, id2, …]`
     * @param {any[]} ops
     * @returns {Float64Array}
     */
    writeBatch(ops) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passArrayJsValueToWasm0(ops, wasm.__wbindgen_export);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostosengine_writeBatch(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            var v2 = getArrayF64FromWasm0(r0, r1).slice();
            wasm.__wbindgen_export5(r0, r1 * 8, 8);
            return v2;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
}
if (Symbol.dispose) NostosEngine.prototype[Symbol.dispose] = NostosEngine.prototype.free;

/**
 * A live WebSocket sync session in the browser.
 *
 * Construct with [`NostosSocket::connect`], which returns a `Promise` that
 * resolves to the socket once the browser has opened it and the subscribe
 * frame is queued (sent on `open`). The server then streams events; each
 * inbound message is decoded by the pure frame-pump, applied to the socket's
 * engine, ACKed per committed batch, and the resulting checkpoint is
 * persisted under the `nostos:checkpoint:<table>` key so a reload can resume —
 * to `localStorage` by default, or to whatever store was injected via
 * [`set_kv_store`] (plan 6.1: the SW-compatible KV seam).
 *
 * ## Resume
 *
 * On `connect`, `resume_lsn` is read from `localStorage` (falling back to 0)
 * and attached to the subscribe frame. The server skips re-delivering anything
 * ≤ that LSN.
 *
 * ## What's NOT durable (ponytail)
 *
 * Only the checkpoint survives a reload — the applied rows live in the
 * engine's `InMemoryStorage` and are lost on reload, so a reconnect replays
 * from `resume_lsn`. Durable rows arrive with OPFS post-v0.1 (ADR-0017).
 *
 * ## JS
 *
 * ```js
 * const sock = await NostosSocket.connect(
 *   "ws://localhost:8080/sync", "tok", "tasks", "priority > 5"
 * );
 * // rows flow in; checkpoint persists to localStorage["nostos:checkpoint:tasks"]
 * console.log(sock.checkpoint, sock.rowCount);
 * sock.close();
 * ```
 */
export class NostosSocket {
    static __wrap(ptr) {
        const obj = Object.create(NostosSocket.prototype);
        obj.__wbg_ptr = ptr;
        NostosSocketFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        NostosSocketFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_nostossocket_free(ptr, 0);
    }
    /**
     * Materialize the WS2 read-views over `nostos_data` on the socket's engine.
     * Delegates to [`NostosEngine::apply_schema`]. SqliteWasm only.
     * @param {any[]} tables
     */
    applySchema(tables) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passArrayJsValueToWasm0(tables, wasm.__wbindgen_export);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostossocket_applySchema(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The current durable checkpoint (the LSN persisted to `localStorage`).
     * Mirrors `NostosEngine::checkpoint`.
     * @returns {number}
     */
    get checkpoint() {
        const ret = wasm.nostossocket_checkpoint(this.__wbg_ptr);
        return ret;
    }
    /**
     * ADR-0029 D1: wipe the socket's engine rows + outbox (sign-out). Call
     * before [`Self::close`]. Mirrors `nostos_client::SyncClient::clear_local_state`.
     */
    clearLocalState() {
        wasm.nostossocket_clearLocalState(this.__wbg_ptr);
    }
    /**
     * Close the socket. The server treats this as a session end; the client
     * keeps its checkpoint so the next `connect` resumes.
     */
    close() {
        wasm.nostossocket_close(this.__wbg_ptr);
    }
    /**
     * Connect to `url`, await the browser's `open`, then resolve. JS sees an
     * `async` fn, so `await NostosSocket.connect(...)` returns the ready socket.
     * The subscribe frame is sent in the `onopen` handler; inbound frames flow
     * into the socket's engine, are acked per committed batch, and the
     * checkpoint is persisted to `localStorage[nostos:checkpoint:<table>]`.
     *
     * `token` is appended as `?token=` on the URL (browsers can't set headers
     * on a WS handshake — same convention as the native `SyncClient`).
     * `table` is the table to subscribe; `where_sql` is the optional safe-SQL
     * predicate (cleared if empty/`null`). `resume_lsn` is read from
     * `localStorage[nostos:checkpoint:<table>]`, falling back to 0.
     *
     * # Errors
     * The `Promise` rejects if the browser can't open the socket (e.g. mixed
     * content) or the handshake fails before OPEN.
     * @param {string} url
     * @param {string | null | undefined} token
     * @param {string} table
     * @param {string | null} [where_sql]
     * @param {object | null} [db_handle]
     * @returns {Promise<NostosSocket>}
     */
    static connect(url, token, table, where_sql, db_handle) {
        const ptr0 = passStringToWasm0(url, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len0 = WASM_VECTOR_LEN;
        var ptr1 = isLikeNone(token) ? 0 : passStringToWasm0(token, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len1 = WASM_VECTOR_LEN;
        const ptr2 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        const len2 = WASM_VECTOR_LEN;
        var ptr3 = isLikeNone(where_sql) ? 0 : passStringToWasm0(where_sql, wasm.__wbindgen_export, wasm.__wbindgen_export2);
        var len3 = WASM_VECTOR_LEN;
        const ret = wasm.nostossocket_connect(ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3, isLikeNone(db_handle) ? 0 : addHeapObject(db_handle));
        return takeObject(ret);
    }
    /**
     * Decrement the PN-Counter by `delta` (bumps the negative counter `n`).
     * Delegates to [`NostosEngine::counter_decrement`].
     * @param {string} table
     * @param {string} pk
     * @param {number} delta
     * @returns {number}
     */
    counterDecrement(table, pk, delta) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            wasm.nostossocket_counterDecrement(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, delta);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Increment the PN-Counter in row `pk` of `table` by `delta` (read-modify-
     * write). Delegates to [`NostosEngine::counter_increment`] (reads the current
     * payload, applies the delta to this replica's entry via `nostos-domain`'s
     * `counter_apply_delta`, enqueues, `apply_local`s) then ships if OPEN.
     * @param {string} table
     * @param {string} pk
     * @param {number} delta
     * @returns {number}
     */
    counterIncrement(table, pk, delta) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            wasm.nostossocket_counterIncrement(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, delta);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The current dead-lettered write count. For `watchWriteStatus`.
     * @returns {number}
     */
    get deadLetteredCount() {
        const ret = wasm.nostossocket_deadLetteredCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * The last error from the most recent dead-lettered write (or null).
     * @returns {string | undefined}
     */
    get lastError() {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.nostossocket_lastError(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            let v1;
            if (r0 !== 0) {
                v1 = getStringFromWasm0(r0, r1).slice();
                wasm.__wbindgen_export5(r0, r1 * 1, 1);
            }
            return v1;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Unregister the reactive callback. Drops the wrapped `Closure` (detaches
     * the JS function). Idempotent. Dropping the socket also cleans up.
     */
    offChange() {
        wasm.nostossocket_offChange(this.__wbg_ptr);
    }
    /**
     * Register a reactive callback nostos invokes on EVERY change tick — the
     * initial snapshot plus each delta — as the browser applies inbound WS
     * frames. This is the Web port of node's `watch(onSnapshot)` / kotlin's
     * `watch(SnapshotSink)` / Flutter's `watch(rows_sink)`: a TRUE Rust→JS push
     * fired synchronously from the `onmessage` frame-pump on each commit, NOT a
     * `setInterval` poll of `rowCount`.
     *
     * The callback receives NO args — it is a change *tick*. Read the fresh
     * full-table snapshot via [`Self::rows_for`] inside the callback (the
     * Worker host does exactly this, then forwards the rows to the main
     * thread). This mirrors the engine's "full snapshot on every tick,
     * self-healing on lag" contract (idempotent) and keeps the FFI boundary
     * free of per-row marshalling.
     *
     * Registering replaces any prior callback (the old `Closure` is dropped →
     * its JS function detached). The `Closure` is owned by the socket; call
     * [`Self::off_change`] to stop, or simply drop the socket — no `.forget()`,
     * so no leak (the one wasm-bindgen `Closure` pitfall).
     *
     * # Initial snapshot
     *
     * Fires the tick once on registration so a UI renders before the first WS
     * frame commits. There is no subscribe-before-snapshot hazard here (the
     * one the node/kotlin ports guard): the WASM pump IS the change tick, so a
     * frame committed before registration is already in `rows_for`, and the
     * initial tick's read sees it.
     *
     * JS:
     * ```js
     * sock.onChange(() => {
     *   console.log("rows changed:", sock.rowsFor("tasks"));
     * });
     * ```
     * @param {Function} callback
     */
    onChange(callback) {
        wasm.nostossocket_onChange(this.__wbg_ptr, addHeapObject(callback));
    }
    /**
     * Add `element` to the add-wins OR-set in row `pk` of `table`. Delegates to
     * [`NostosEngine::or_set_add`] (mints the client HLC, builds the
     * `OrSetPayload`, enqueues, `apply_local`s) then ships the write frame now
     * if the socket is OPEN. Mirrors [`Self::write`]'s enqueue→apply→ship→tick
     * flow. Returns the outbox id (ADR-0032 T4 / ADR-0030).
     * @param {string} table
     * @param {string} pk
     * @param {string} element
     * @returns {number}
     */
    orSetAdd(table, pk, element) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(element, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len2 = WASM_VECTOR_LEN;
            wasm.nostossocket_orSetAdd(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Remove `element` from the OR-set (a tombstone at a fresh HLC). Add-wins:
     * a concurrent or later re-add re-activates the element. Delegates to
     * [`NostosEngine::or_set_remove`].
     * @param {string} table
     * @param {string} pk
     * @param {string} element
     * @returns {number}
     */
    orSetRemove(table, pk, element) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(element, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len2 = WASM_VECTOR_LEN;
            wasm.nostossocket_orSetRemove(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The current pending (non-dead-lettered) write count. For
     * `watchWriteStatus`.
     * @returns {number}
     */
    get pendingCount() {
        const ret = wasm.nostossocket_pendingCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Run an arbitrary SELECT, returning a JSON-array string. Delegates to
     * [`NostosEngine::query`]. SqliteWasm only (Memory returns `"[]"`).
     * @param {string} sql
     * @returns {string}
     */
    query(sql) {
        let deferred3_0;
        let deferred3_1;
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(sql, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostossocket_query(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            var ptr2 = r0;
            var len2 = r1;
            if (r3) {
                ptr2 = 0; len2 = 0;
                throw takeObject(r2);
            }
            deferred3_0 = ptr2;
            deferred3_1 = len2;
            return getStringFromWasm0(ptr2, len2);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
            wasm.__wbindgen_export5(deferred3_0, deferred3_1, 1);
        }
    }
    /**
     * Re-assert the subscription on an already-open socket (heartbeat
     * re-send of the subscribe frame with the persisted checkpoint;
     * returns `false`). If the socket is CLOSED this returns `Err` —
     * call `connect()` instead. (Audit 2026-08-17 L3: an earlier version of
     * this doc promised a socket-creating resume returning `true` — never
     * implemented. The `ws` field is not interior-mutable, so an in-place
     * hot-swap would ripple through the transport; that refactor is the
     * upgrade path for a true resume.) The engine (rows, checkpoint,
     * outbox) survives a reconnect — the server resumes streaming from
     * the persisted checkpoint.
     * @returns {Promise<boolean>}
     */
    resume() {
        const ret = wasm.nostossocket_resume(this.__wbg_ptr);
        return takeObject(ret);
    }
    /**
     * Rows the in-memory store currently holds. Mirrors `NostosEngine::row_count`.
     * @returns {number}
     */
    get rowCount() {
        const ret = wasm.nostossocket_rowCount(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Enumerate the `(pk, payload)` pairs the socket's engine holds for
     * `table`. Mirrors `NostosEngine::rows_for` — the readback the demo renders
     * from. Safe because WASM is single-threaded and the JS event loop is
     * cooperative — `setInterval(snapshot, …)` and the WS `onmessage` pump
     * never run concurrently, so the `borrow_mut()` in the pump
     * (`transport.rs`) and this `borrow()` can't overlap (a `RefCell` panics
     * on re-borrow mid-`borrow_mut`; it doesn't deadlock, but the
     * cooperative-event-loop invariant is what keeps that from happening).
     * @param {string} table
     * @returns {RowEntry[]}
     */
    rowsFor(table) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostossocket_rowsFor(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var v2 = getArrayJsValueFromWasm0(r0, r1).slice();
            wasm.__wbindgen_export5(r0, r1 * 4, 4);
            return v2;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Configure which tables are OR-set / counter CRDTs on the socket's engine.
     * Delegates to [`NostosEngine::set_crdt_tables`].
     * @param {string[]} or_set
     * @param {string[]} counter
     */
    setCrdtTables(or_set, counter) {
        const ptr0 = passArrayJsValueToWasm0(or_set, wasm.__wbindgen_export);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passArrayJsValueToWasm0(counter, wasm.__wbindgen_export);
        const len1 = WASM_VECTOR_LEN;
        wasm.nostossocket_setCrdtTables(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    }
    /**
     * Send an additional subscribe frame for a DIFFERENT table over the
     * existing socket (Wave 4a multi-table subscribe). The server streams
     * events for all subscribed tables over the same socket. Call AFTER
     * `connect` resolves.
     *
     * ponytail: the current transport is single-table at the engine level
     * (the frame-pump acks/persists per-table). A true multi-table port needs
     * per-table checkpoint tracking in the engine — the server sends events
     * tagged by `table`, and each table has its own resume_lsn. For now, the
     * subscribe frame is sent but the checkpoint persists for the FIRST table
     * only. rewire when convenient.
     * @param {string} table
     * @param {string | null} [where_sql]
     */
    subscribe(table, where_sql) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            var ptr1 = isLikeNone(where_sql) ? 0 : passStringToWasm0(where_sql, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            var len1 = WASM_VECTOR_LEN;
            wasm.nostossocket_subscribe(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            if (r1) {
                throw takeObject(r0);
            }
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Send a client write. WS1 contract: this NEVER throws because the socket
     * is closed — a write while disconnected is captured into the `Outbox`
     * (`enqueue`) and rendered locally right away (`apply_local`), so the row
     * is visible INSTANTLY and the write ships on the next (re)connect via the
     * `onopen` flush loop. The synchronous "socket not OPEN" throw is gone
     * (ADR-0017 WS1; reviewer note #1).
     *
     * The call is still `Err` for a *caller bug* — a malformed / non-object
     * `payload_json`, or an `op` outside `"upsert" | "delete" | "patch"`. Those
     * return BEFORE anything is enqueued (an invalid write is not captured).
     *
     * When the socket IS open, the write ships immediately and is
     * `mark_done`'d, so the connected path keeps the outbox drained. The
     * caller learns the outcome asynchronously: the Worker host turns the
     * `Ok(())` into a `writeResult{client_write_id, ok:true}` push (Rust can't
     * `postMessage` to the main thread itself).
     *
     * `client_write_id` is the caller's correlation id, put on the wire when
     * the write ships now. The offline flush loop synthesizes one from the
     * outbox id (ponytail: `PendingWrite` — a `nostos-core` domain type —
     * carries no `client_write_id` field, so the caller's id is lost across an
     * offline gap; the live path preserves it).
     * Send a client write. WS1 contract: this NEVER throws because the socket
     * is closed — a write while disconnected is captured into the `Outbox`
     * (`enqueue`) and rendered locally right away (`apply_local`), so the row
     * is visible INSTANTLY and the write ships on the next (re)connect via the
     * `onopen` flush loop. Returns the outbox id (Wave 4a: mirrors native
     * `write` returning the id — the caller can use it to correlate with
     * `watchWriteStatus` outcomes).
     *
     * The call is still `Err` for a *caller bug* — a malformed / non-object
     * `payload_json`, or an `op` outside `"upsert" | "delete" | "patch"`.
     * @param {string} table
     * @param {string} op
     * @param {string} pk
     * @param {string | null | undefined} payload_json
     * @param {string} client_write_id
     * @returns {number}
     */
    write(table, op, pk, payload_json, client_write_id) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passStringToWasm0(table, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len0 = WASM_VECTOR_LEN;
            const ptr1 = passStringToWasm0(op, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            const ptr2 = passStringToWasm0(pk, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len2 = WASM_VECTOR_LEN;
            var ptr3 = isLikeNone(payload_json) ? 0 : passStringToWasm0(payload_json, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            var len3 = WASM_VECTOR_LEN;
            const ptr4 = passStringToWasm0(client_write_id, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len4 = WASM_VECTOR_LEN;
            wasm.nostossocket_write(retptr, this.__wbg_ptr, ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3, ptr4, len4);
            var r0 = getDataViewMemory0().getFloat64(retptr + 8 * 0, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            return r0;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * Enqueue a batch of writes atomically (ADR-0032 T3). All ops commit in
     * one SQLite txn (SqliteWasm) or one BTreeMap extend (Memory) via the
     * engine's `enqueue_batch` — a mid-batch failure rolls back the entire
     * batch. Delegates to [`NostosEngine::write_batch`], then ships each write
     * now if OPEN. Returns the outbox ids in order.
     * @param {any[]} ops
     * @returns {Float64Array}
     */
    writeBatch(ops) {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            const ptr0 = passArrayJsValueToWasm0(ops, wasm.__wbindgen_export);
            const len0 = WASM_VECTOR_LEN;
            wasm.nostossocket_writeBatch(retptr, this.__wbg_ptr, ptr0, len0);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var r2 = getDataViewMemory0().getInt32(retptr + 4 * 2, true);
            var r3 = getDataViewMemory0().getInt32(retptr + 4 * 3, true);
            if (r3) {
                throw takeObject(r2);
            }
            var v2 = getArrayF64FromWasm0(r0, r1).slice();
            wasm.__wbindgen_export5(r0, r1 * 8, 8);
            return v2;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
}
if (Symbol.dispose) NostosSocket.prototype[Symbol.dispose] = NostosSocket.prototype.free;

/**
 * The result of an atomic commit, mirrored to JS.
 */
export class Outcome {
    static __wrap(ptr) {
        const obj = Object.create(Outcome.prototype);
        obj.__wbg_ptr = ptr;
        OutcomeFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        OutcomeFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_outcome_free(ptr, 0);
    }
    /**
     * The new durable checkpoint — the value to `Ack` to the server.
     * @returns {number}
     */
    get checkpoint() {
        const ret = wasm.outcome_checkpoint(this.__wbg_ptr);
        return ret;
    }
    /**
     * Rows applied in this commit.
     * @returns {number}
     */
    get rowsApplied() {
        const ret = wasm.outcome_rowsApplied(this.__wbg_ptr);
        return ret >>> 0;
    }
}
if (Symbol.dispose) Outcome.prototype[Symbol.dispose] = Outcome.prototype.free;

/**
 * One `(pk, payload)` pair returned by [`NostosEngine::rows_for`]. The JS-facing
 * projection of `InMemoryStorage`'s readback — `pk` is the row's primary key,
 * `payload` is the opaque tuple image (the bytes the engine applied), exposed
 * as a `Uint8Array` (matches the `Frame` payload convention).
 *
 * Not constructable from JS: instances only flow OUT of the engine (the engine
 * is the source of truth for row state). JS reads `entry.pk` / `entry.payload`.
 */
export class RowEntry {
    static __wrap(ptr) {
        const obj = Object.create(RowEntry.prototype);
        obj.__wbg_ptr = ptr;
        RowEntryFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        RowEntryFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_rowentry_free(ptr, 0);
    }
    /**
     * The opaque tuple image bytes (decode/interpret on the JS side).
     * @returns {Uint8Array}
     */
    get payload() {
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.rowentry_payload(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            var v1 = getArrayU8FromWasm0(r0, r1).slice();
            wasm.__wbindgen_export5(r0, r1 * 1, 1);
            return v1;
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
        }
    }
    /**
     * The row's primary key.
     * @returns {string}
     */
    get pk() {
        let deferred1_0;
        let deferred1_1;
        try {
            const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
            wasm.rowentry_pk(retptr, this.__wbg_ptr);
            var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
            var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
            deferred1_0 = r0;
            deferred1_1 = r1;
            return getStringFromWasm0(r0, r1);
        } finally {
            wasm.__wbindgen_add_to_stack_pointer(16);
            wasm.__wbindgen_export5(deferred1_0, deferred1_1, 1);
        }
    }
}
if (Symbol.dispose) RowEntry.prototype[Symbol.dispose] = RowEntry.prototype.free;

/**
 * Inject the key-value store the transport persists sync checkpoints to
 * (plan task 6.1 / ADR-0037 §6 Wave 3 — EXPERIMENTAL Web Push enablement).
 *
 * `store` is any JS object with the Web Storage shape — `getItem(key)`
 * returning a string or null, and `setItem(key, value)`. Pass `localStorage`
 * itself (the default when unset), a Map-backed shim (the Service-Worker
 * context has no `window`), or a test spy. Passing `null`/`undefined`
 * restores the default. Call BEFORE `NostosSocket.connect` — the active store
 * is captured per-socket at connect time. Default behavior for embedders
 * that never call this is unchanged (`window.localStorage`, a no-op where no
 * window exists).
 * @param {object | null} [store]
 */
export function setKvStore(store) {
    wasm.setKvStore(isLikeNone(store) ? 0 : addHeapObject(store));
}
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg___wbindgen_debug_string_c25d447a39f5578f: function(arg0, arg1) {
            const ret = debugString(getObject(arg1));
            const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            const len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg___wbindgen_is_function_1ff95bcc5517c252: function(arg0) {
            const ret = typeof(getObject(arg0)) === 'function';
            return ret;
        },
        __wbg___wbindgen_is_null_ea9085d691f535d3: function(arg0) {
            const ret = getObject(arg0) === null;
            return ret;
        },
        __wbg___wbindgen_is_string_ea5e6cc2e4141dfe: function(arg0) {
            const ret = typeof(getObject(arg0)) === 'string';
            return ret;
        },
        __wbg___wbindgen_is_undefined_c05833b95a3cf397: function(arg0) {
            const ret = getObject(arg0) === undefined;
            return ret;
        },
        __wbg___wbindgen_number_get_394265ed1e1b84ee: function(arg0, arg1) {
            const obj = getObject(arg1);
            const ret = typeof(obj) === 'number' ? obj : undefined;
            getDataViewMemory0().setFloat64(arg0 + 8 * 1, isLikeNone(ret) ? 0 : ret, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
        },
        __wbg___wbindgen_string_get_b0ca35b86a603356: function(arg0, arg1) {
            const obj = getObject(arg1);
            const ret = typeof(obj) === 'string' ? obj : undefined;
            var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            var len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbg___wbindgen_throw_344f42d3211c4765: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg__wbg_cb_unref_fffb441def202758: function(arg0) {
            getObject(arg0)._wbg_cb_unref();
        },
        __wbg_apply_3ac86a26fdb56c05: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = getObject(arg0).apply(getObject(arg1), getObject(arg2));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_call_8a2dd23819f8a60a: function() { return handleError(function (arg0, arg1) {
            const ret = getObject(arg0).call(getObject(arg1));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_call_a6e5c5dce5018821: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = getObject(arg0).call(getObject(arg1), getObject(arg2));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_call_e3b662382210db98: function() { return handleError(function (arg0, arg1, arg2, arg3) {
            const ret = getObject(arg0).call(getObject(arg1), getObject(arg2), getObject(arg3));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_close_c65ca0257e895318: function() { return handleError(function (arg0) {
            getObject(arg0).close();
        }, arguments); },
        __wbg_close_d820db467b05a96a: function() { return handleError(function (arg0, arg1) {
            getObject(arg0).close(arg1);
        }, arguments); },
        __wbg_data_328de4280640da92: function(arg0) {
            const ret = getObject(arg0).data;
            return addHeapObject(ret);
        },
        __wbg_from_13e323c65fc8f464: function(arg0) {
            const ret = Array.from(getObject(arg0));
            return addHeapObject(ret);
        },
        __wbg_getItem_b96269ddc16cf24a: function() { return handleError(function (arg0, arg1, arg2, arg3) {
            const ret = getObject(arg1).getItem(getStringFromWasm0(arg2, arg3));
            var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_export, wasm.__wbindgen_export2);
            var len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        }, arguments); },
        __wbg_get_507a50627bffa49b: function(arg0, arg1) {
            const ret = getObject(arg0)[arg1 >>> 0];
            return addHeapObject(ret);
        },
        __wbg_get_78f252d074a84d0b: function() { return handleError(function (arg0, arg1) {
            const ret = Reflect.get(getObject(arg0), getObject(arg1));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_instanceof_ArrayBuffer_4480b9e0068a8adb: function(arg0) {
            let result;
            try {
                result = getObject(arg0) instanceof ArrayBuffer;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_instanceof_Window_05ba1ee4f6781663: function(arg0) {
            let result;
            try {
                result = getObject(arg0) instanceof Window;
            } catch (_) {
                result = false;
            }
            const ret = result;
            return ret;
        },
        __wbg_length_1f0964f4a5e2c6d8: function(arg0) {
            const ret = getObject(arg0).length;
            return ret;
        },
        __wbg_length_370319915dc99107: function(arg0) {
            const ret = getObject(arg0).length;
            return ret;
        },
        __wbg_localStorage_5bf6ce3f8e51412a: function() { return handleError(function (arg0) {
            const ret = getObject(arg0).localStorage;
            return isLikeNone(ret) ? 0 : addHeapObject(ret);
        }, arguments); },
        __wbg_new_32b398fb48b6d94a: function() {
            const ret = new Array();
            return addHeapObject(ret);
        },
        __wbg_new_aec3e25493d729fe: function(arg0, arg1) {
            try {
                var state0 = {a: arg0, b: arg1};
                var cb0 = (arg0, arg1) => {
                    const a = state0.a;
                    state0.a = 0;
                    try {
                        return __wasm_bindgen_func_elem_582(a, state0.b, arg0, arg1);
                    } finally {
                        state0.a = a;
                    }
                };
                const ret = new Promise(cb0);
                return addHeapObject(ret);
            } finally {
                state0.a = 0;
            }
        },
        __wbg_new_bf8729ffe10e9ee7: function() { return handleError(function (arg0, arg1) {
            const ret = new WebSocket(getStringFromWasm0(arg0, arg1));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_new_cd45aabdf6073e84: function(arg0) {
            const ret = new Uint8Array(getObject(arg0));
            return addHeapObject(ret);
        },
        __wbg_new_da52cf8fe3429cb2: function() {
            const ret = new Object();
            return addHeapObject(ret);
        },
        __wbg_new_from_slice_77cdfb7977362f3c: function(arg0, arg1) {
            const ret = new Uint8Array(getArrayU8FromWasm0(arg0, arg1));
            return addHeapObject(ret);
        },
        __wbg_new_typed_1824d93f294193e5: function(arg0, arg1) {
            try {
                var state0 = {a: arg0, b: arg1};
                var cb0 = (arg0, arg1) => {
                    const a = state0.a;
                    state0.a = 0;
                    try {
                        return __wasm_bindgen_func_elem_582(a, state0.b, arg0, arg1);
                    } finally {
                        state0.a = a;
                    }
                };
                const ret = new Promise(cb0);
                return addHeapObject(ret);
            } finally {
                state0.a = 0;
            }
        },
        __wbg_nostossocket_new: function(arg0) {
            const ret = NostosSocket.__wrap(arg0);
            return addHeapObject(ret);
        },
        __wbg_now_86c0d4ba3fa605b8: function() {
            const ret = Date.now();
            return ret;
        },
        __wbg_prototypesetcall_4770620bbe4688a0: function(arg0, arg1, arg2) {
            Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), getObject(arg2));
        },
        __wbg_push_d2ae3af0c1217ae6: function(arg0, arg1) {
            const ret = getObject(arg0).push(getObject(arg1));
            return ret;
        },
        __wbg_queueMicrotask_0ab5b2d2393e99b9: function(arg0) {
            const ret = getObject(arg0).queueMicrotask;
            return addHeapObject(ret);
        },
        __wbg_queueMicrotask_6a09b7bc46549209: function(arg0) {
            queueMicrotask(getObject(arg0));
        },
        __wbg_readyState_50bc38c2a9e83db6: function(arg0) {
            const ret = getObject(arg0).readyState;
            return ret;
        },
        __wbg_resolve_2191a4dfe481c25b: function(arg0) {
            const ret = Promise.resolve(getObject(arg0));
            return addHeapObject(ret);
        },
        __wbg_rowentry_new: function(arg0) {
            const ret = RowEntry.__wrap(arg0);
            return addHeapObject(ret);
        },
        __wbg_send_df98dd5ede9b3f4d: function() { return handleError(function (arg0, arg1, arg2) {
            getObject(arg0).send(getStringFromWasm0(arg1, arg2));
        }, arguments); },
        __wbg_setItem_364a11cf21db9039: function() { return handleError(function (arg0, arg1, arg2, arg3, arg4) {
            getObject(arg0).setItem(getStringFromWasm0(arg1, arg2), getStringFromWasm0(arg3, arg4));
        }, arguments); },
        __wbg_set_8535240470bf2500: function() { return handleError(function (arg0, arg1, arg2) {
            const ret = Reflect.set(getObject(arg0), getObject(arg1), getObject(arg2));
            return ret;
        }, arguments); },
        __wbg_set_binaryType_a37b086c78ca7c29: function(arg0, arg1) {
            getObject(arg0).binaryType = __wbindgen_enum_BinaryType[arg1];
        },
        __wbg_set_onclose_f706475385ecce07: function(arg0, arg1) {
            getObject(arg0).onclose = getObject(arg1);
        },
        __wbg_set_onerror_9f5773fd31512333: function(arg0, arg1) {
            getObject(arg0).onerror = getObject(arg1);
        },
        __wbg_set_onmessage_836d2f72130b4706: function(arg0, arg1) {
            getObject(arg0).onmessage = getObject(arg1);
        },
        __wbg_set_onopen_4f65470ae522a61a: function(arg0, arg1) {
            getObject(arg0).onopen = getObject(arg1);
        },
        __wbg_static_accessor_GLOBAL_4ef717fb391d88b7: function() {
            const ret = typeof global === 'undefined' ? null : global;
            return isLikeNone(ret) ? 0 : addHeapObject(ret);
        },
        __wbg_static_accessor_GLOBAL_THIS_8d1badc68b5a74f4: function() {
            const ret = typeof globalThis === 'undefined' ? null : globalThis;
            return isLikeNone(ret) ? 0 : addHeapObject(ret);
        },
        __wbg_static_accessor_SELF_146583524fe1469b: function() {
            const ret = typeof self === 'undefined' ? null : self;
            return isLikeNone(ret) ? 0 : addHeapObject(ret);
        },
        __wbg_static_accessor_WINDOW_f2829a2234d7819e: function() {
            const ret = typeof window === 'undefined' ? null : window;
            return isLikeNone(ret) ? 0 : addHeapObject(ret);
        },
        __wbg_stringify_b54333f60f1e4dad: function() { return handleError(function (arg0) {
            const ret = JSON.stringify(getObject(arg0));
            return addHeapObject(ret);
        }, arguments); },
        __wbg_then_16d107c451e9905d: function(arg0, arg1, arg2) {
            const ret = getObject(arg0).then(getObject(arg1), getObject(arg2));
            return addHeapObject(ret);
        },
        __wbg_then_6ec10ae38b3e92f7: function(arg0, arg1) {
            const ret = getObject(arg0).then(getObject(arg1));
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000001: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [Externref], shim_idx: 5, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, __wasm_bindgen_func_elem_148);
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000002: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [Externref], shim_idx: 81, ret: Result(Unit), inner_ret: Some(Result(Unit)) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, __wasm_bindgen_func_elem_568);
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000003: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [NamedExternref("CloseEvent")], shim_idx: 5, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, __wasm_bindgen_func_elem_148_2);
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000004: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [NamedExternref("ErrorEvent")], shim_idx: 5, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, __wasm_bindgen_func_elem_148_3);
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000005: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 5, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, __wasm_bindgen_func_elem_148_4);
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000006: function(arg0, arg1) {
            // Cast intrinsic for `Closure(Closure { owned: true, function: Function { arguments: [], shim_idx: 10, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
            const ret = makeMutClosure(arg0, arg1, __wasm_bindgen_func_elem_153);
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000007: function(arg0) {
            // Cast intrinsic for `F64 -> Externref`.
            const ret = arg0;
            return addHeapObject(ret);
        },
        __wbindgen_cast_0000000000000008: function(arg0, arg1) {
            // Cast intrinsic for `Ref(String) -> Externref`.
            const ret = getStringFromWasm0(arg0, arg1);
            return addHeapObject(ret);
        },
        __wbindgen_object_clone_ref: function(arg0) {
            const ret = getObject(arg0);
            return addHeapObject(ret);
        },
        __wbindgen_object_drop_ref: function(arg0) {
            takeObject(arg0);
        },
    };
    return {
        __proto__: null,
        "./nostos_ffi_wasm_bg.js": import0,
    };
}

function __wasm_bindgen_func_elem_153(arg0, arg1) {
    wasm.__wasm_bindgen_func_elem_153(arg0, arg1);
}

function __wasm_bindgen_func_elem_148(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_148(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_148_2(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_148_2(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_148_3(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_148_3(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_148_4(arg0, arg1, arg2) {
    wasm.__wasm_bindgen_func_elem_148_4(arg0, arg1, addHeapObject(arg2));
}

function __wasm_bindgen_func_elem_568(arg0, arg1, arg2) {
    try {
        const retptr = wasm.__wbindgen_add_to_stack_pointer(-16);
        wasm.__wasm_bindgen_func_elem_568(retptr, arg0, arg1, addHeapObject(arg2));
        var r0 = getDataViewMemory0().getInt32(retptr + 4 * 0, true);
        var r1 = getDataViewMemory0().getInt32(retptr + 4 * 1, true);
        if (r1) {
            throw takeObject(r0);
        }
    } finally {
        wasm.__wbindgen_add_to_stack_pointer(16);
    }
}

function __wasm_bindgen_func_elem_582(arg0, arg1, arg2, arg3) {
    wasm.__wasm_bindgen_func_elem_582(arg0, arg1, addHeapObject(arg2), addHeapObject(arg3));
}


const __wbindgen_enum_BinaryType = ["blob", "arraybuffer"];
const FrameFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_frame_free(ptr, 1));
const NostosEngineFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_nostosengine_free(ptr, 1));
const NostosSocketFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_nostossocket_free(ptr, 1));
const OutcomeFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_outcome_free(ptr, 1));
const RowEntryFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_rowentry_free(ptr, 1));

function addHeapObject(obj) {
    if (heap_next === heap.length) heap.push(heap.length + 1);
    const idx = heap_next;
    heap_next = heap[idx];

    heap[idx] = obj;
    return idx;
}

function _assertClass(instance, klass) {
    if (!(instance instanceof klass)) {
        throw new Error(`expected instance of ${klass.name}`);
    }
}

const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(state => wasm.__wbindgen_export4(state.a, state.b));

function debugString(val) {
    // primitive types
    const type = typeof val;
    if (type == 'number' || type == 'boolean' || val == null) {
        return  `${val}`;
    }
    if (type == 'string') {
        return `"${val}"`;
    }
    if (type == 'symbol') {
        const description = val.description;
        if (description == null) {
            return 'Symbol';
        } else {
            return `Symbol(${description})`;
        }
    }
    if (type == 'function') {
        const name = val.name;
        if (typeof name == 'string' && name.length > 0) {
            return `Function(${name})`;
        } else {
            return 'Function';
        }
    }
    // objects
    if (Array.isArray(val)) {
        const length = val.length;
        let debug = '[';
        if (length > 0) {
            debug += debugString(val[0]);
        }
        for(let i = 1; i < length; i++) {
            debug += ', ' + debugString(val[i]);
        }
        debug += ']';
        return debug;
    }
    // Test for built-in
    const builtInMatches = /\[object ([^\]]+)\]/.exec(toString.call(val));
    let className;
    if (builtInMatches && builtInMatches.length > 1) {
        className = builtInMatches[1];
    } else {
        // Failed to match the standard '[object ClassName]'
        return toString.call(val);
    }
    if (className == 'Object') {
        // we're a user defined class or Object
        // JSON.stringify avoids problems with cycles, and is generally much
        // easier than looping through ownProperties of `val`.
        try {
            return 'Object(' + JSON.stringify(val) + ')';
        } catch (_) {
            return 'Object';
        }
    }
    // errors
    if (val instanceof Error) {
        return `${val.name}: ${val.message}\n${val.stack}`;
    }
    // TODO we could test for more things here, like `Set`s and `Map`s.
    return className;
}

function dropObject(idx) {
    if (idx < 1028) return;
    heap[idx] = heap_next;
    heap_next = idx;
}

function getArrayF64FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getFloat64ArrayMemory0().subarray(ptr / 8, ptr / 8 + len);
}

function getArrayJsValueFromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    const mem = getDataViewMemory0();
    const result = [];
    for (let i = ptr; i < ptr + 4 * len; i += 4) {
        result.push(takeObject(mem.getUint32(i, true)));
    }
    return result;
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

let cachedFloat64ArrayMemory0 = null;
function getFloat64ArrayMemory0() {
    if (cachedFloat64ArrayMemory0 === null || cachedFloat64ArrayMemory0.byteLength === 0) {
        cachedFloat64ArrayMemory0 = new Float64Array(wasm.memory.buffer);
    }
    return cachedFloat64ArrayMemory0;
}

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function getObject(idx) { return heap[idx]; }

function handleError(f, args) {
    try {
        return f.apply(this, args);
    } catch (e) {
        wasm.__wbindgen_export3(addHeapObject(e));
    }
}

let heap = new Array(1024).fill(undefined);
heap.push(undefined, null, true, false);

let heap_next = heap.length;

function isLikeNone(x) {
    return x === undefined || x === null;
}

function makeMutClosure(arg0, arg1, f) {
    const state = { a: arg0, b: arg1, cnt: 1 };
    const real = (...args) => {

        // First up with a closure we increment the internal reference
        // count. This ensures that the Rust closure environment won't
        // be deallocated while we're invoking it.
        state.cnt++;
        const a = state.a;
        state.a = 0;
        try {
            return f(a, state.b, ...args);
        } finally {
            state.a = a;
            real._wbg_cb_unref();
        }
    };
    real._wbg_cb_unref = () => {
        if (--state.cnt === 0) {
            wasm.__wbindgen_export4(state.a, state.b);
            state.a = 0;
            CLOSURE_DTORS.unregister(state);
        }
    };
    CLOSURE_DTORS.register(real, state, state);
    return real;
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passArrayJsValueToWasm0(array, malloc) {
    const ptr = malloc(array.length * 4, 4) >>> 0;
    const mem = getDataViewMemory0();
    for (let i = 0; i < array.length; i++) {
        mem.setUint32(ptr + 4 * i, addHeapObject(array[i]), true);
    }
    WASM_VECTOR_LEN = array.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

function takeObject(idx) {
    const ret = getObject(idx);
    dropObject(idx);
    return ret;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedDataViewMemory0 = null;
    cachedFloat64ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('nostos_ffi_wasm_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
