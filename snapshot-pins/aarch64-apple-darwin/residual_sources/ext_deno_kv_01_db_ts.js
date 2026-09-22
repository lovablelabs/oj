"use strict"; return ((function() {
  const { core, internals, primordials } = __bootstrap;
  const { isPromise } = core;
  const { op_kv_atomic_write, op_kv_database_open, op_kv_dequeue_next_message, op_kv_encode_cursor, op_kv_finish_dequeued_message, op_kv_snapshot_read, op_kv_watch, op_kv_watch_next } = core.ops;
  const { ArrayFrom, ArrayPrototypeJoin, ArrayPrototypeMap, ArrayPrototypePush, ArrayPrototypeReverse, ArrayPrototypeSlice, AsyncGeneratorPrototype, BigInt, BigIntPrototypeToString, Error, NumberIsInteger, NumberIsNaN, Object, ObjectFreeze, ObjectGetPrototypeOf, ObjectHasOwn, ObjectPrototypeIsPrototypeOf, RangeError, SafeMap, SafeMapIterator, StringPrototypeReplace, Symbol, SymbolAsyncIterator, SymbolDispose, SymbolFor, SymbolToStringTag, TypeError, TypedArrayPrototypeGetSymbolToStringTag } = primordials;
  const { ReadableStream } = core.loadExtScript("ext:deno_web/06_streams.js");
  const cloneableDeserializers = core.getCloneableDeserializers();
  const encodeCursor = (selector, boundaryKey)=>op_kv_encode_cursor(selector, boundaryKey);
  async function openKv(path) {
    const rid = await op_kv_database_open(path);
    return new Kv(rid, kvSymbol);
  }
  const maxQueueDelay = 30 * 24 * 60 * 60 * 1000;
  function validateQueueDelay(delay) {
    if (delay < 0) {
      throw new TypeError(`Delay must be >= 0: received ${delay}`);
    }
    if (delay > maxQueueDelay) {
      throw new TypeError(`Delay cannot be greater than 30 days: received ${delay}`);
    }
    if (NumberIsNaN(delay)) {
      throw new TypeError("Delay cannot be NaN");
    }
  }
  function validateExpireIn(expireIn) {
    if (expireIn === undefined) return;
    // Reject NaN, Infinity, fractional and negative values. A non-finite
    // expireIn otherwise reaches the native layer and overflows when computing
    // the absolute expiry, panicking the process.
    if (!NumberIsInteger(expireIn) || expireIn < 0) {
      throw new TypeError(`expireIn must be a non-negative integer: received ${expireIn}`);
    }
  }
  const maxQueueBackoffIntervals = 5;
  const maxQueueBackoffInterval = 60 * 60 * 1000;
  function validateBackoffSchedule(backoffSchedule) {
    if (backoffSchedule.length > maxQueueBackoffIntervals) {
      throw new TypeError(`Invalid backoffSchedule, max ${maxQueueBackoffIntervals} intervals allowed`);
    }
    for(let i = 0; i < backoffSchedule.length; ++i){
      const interval = backoffSchedule[i];
      if (interval < 0 || interval > maxQueueBackoffInterval || NumberIsNaN(interval)) {
        throw new TypeError(`Invalid backoffSchedule, interval at index ${i} is invalid`);
      }
    }
  }
  const kvSymbol = Symbol("KvRid");
  const commitVersionstampSymbol = Symbol("KvCommitVersionstamp");
  class Kv {
    #rid;
    #isClosed;
    constructor(rid = undefined, symbol = undefined){
      if (kvSymbol !== symbol) {
        throw new TypeError("Deno.Kv can not be constructed: use Deno.openKv instead");
      }
      this.#rid = rid;
      this.#isClosed = false;
    }
    atomic() {
      return new AtomicOperation(this.#rid);
    }
    commitVersionstamp() {
      return commitVersionstampSymbol;
    }
    async get(key, opts) {
      const { 0: entries } = await op_kv_snapshot_read(this.#rid, [
        [
          null,
          key,
          null,
          1,
          false,
          null
        ]
      ], opts?.consistency ?? "strong");
      if (!entries.length) {
        return {
          key,
          value: null,
          versionstamp: null
        };
      }
      return deserializeValue(entries[0]);
    }
    async getMany(keys, opts) {
      const ranges = await op_kv_snapshot_read(this.#rid, ArrayPrototypeMap(keys, (key)=>[
          null,
          key,
          null,
          1,
          false,
          null
        ]), opts?.consistency ?? "strong");
      return ArrayPrototypeMap(ranges, (entries, i)=>{
        if (!entries.length) {
          return {
            key: keys[i],
            value: null,
            versionstamp: null
          };
        }
        return deserializeValue(entries[0]);
      });
    }
    async set(key, value, options) {
      validateExpireIn(options?.expireIn);
      const versionstamp = await doAtomicWriteInPlace(this.#rid, [], [
        [
          key,
          "set",
          serializeValue(value),
          options?.expireIn
        ]
      ], []);
      if (versionstamp === null) throw new TypeError("Failed to set value");
      return {
        ok: true,
        versionstamp
      };
    }
    async delete(key) {
      const result = await doAtomicWriteInPlace(this.#rid, [], [
        [
          key,
          "delete",
          null,
          undefined
        ]
      ], []);
      if (!result) throw new TypeError("Failed to set value");
    }
    list(selector, options = {
      __proto__: null
    }) {
      if (options.limit !== undefined && (!NumberIsInteger(options.limit) || options.limit <= 0)) {
        throw new Error(`Limit must be a positive integer: received ${options.limit}`);
      }
      let batchSize = options.batchSize ?? options.limit ?? 100;
      if (!NumberIsInteger(batchSize) || batchSize <= 0) {
        throw new Error(`batchSize must be a positive integer: received ${batchSize}`);
      }
      if (options.batchSize === undefined && batchSize > 500) batchSize = 500;
      return new KvListIterator({
        limit: options.limit,
        selector,
        cursor: options.cursor,
        reverse: options.reverse ?? false,
        consistency: options.consistency ?? "strong",
        batchSize,
        pullBatch: this.#pullBatch(batchSize)
      });
    }
    #pullBatch(batchSize) {
      return async (selector, cursor, reverse, consistency)=>{
        const { 0: entries } = await op_kv_snapshot_read(this.#rid, [
          [
            ObjectHasOwn(selector, "prefix") ? selector.prefix : null,
            ObjectHasOwn(selector, "start") ? selector.start : null,
            ObjectHasOwn(selector, "end") ? selector.end : null,
            batchSize,
            reverse,
            cursor
          ]
        ], consistency);
        return ArrayPrototypeMap(entries, deserializeValue);
      };
    }
    async enqueue(message, opts) {
      if (opts?.delay !== undefined) {
        validateQueueDelay(opts?.delay);
      }
      if (opts?.backoffSchedule !== undefined) {
        validateBackoffSchedule(opts?.backoffSchedule);
      }
      const versionstamp = await doAtomicWriteInPlace(this.#rid, [], [], [
        [
          core.serialize(message, {
            forStorage: true
          }),
          opts?.delay ?? 0,
          opts?.keysIfUndelivered ?? [],
          opts?.backoffSchedule ?? null
        ]
      ]);
      if (versionstamp === null) throw new TypeError("Failed to enqueue value");
      return {
        ok: true,
        versionstamp
      };
    }
    async listenQueue(handler) {
      if (this.#isClosed) {
        throw new Error("Queue already closed");
      }
      const finishMessageOps = new SafeMap();
      while(true){
        // Wait for the next message.
        const next = await op_kv_dequeue_next_message(this.#rid);
        if (next === null) {
          break;
        }
        // Deserialize the payload.
        const { 0: payload, 1: handleId } = next;
        const deserializedPayload = core.deserialize(payload, {
          forStorage: true,
          deserializers: cloneableDeserializers
        });
        // Dispatch the payload.
        (async ()=>{
          let success = false;
          try {
            const result = handler(deserializedPayload);
            const _res = isPromise(result) ? await result : result;
            success = true;
          } catch (error) {
            internals.log("error", "Exception in queue handler", error);
          } finally{
            const promise = op_kv_finish_dequeued_message(handleId, success);
            finishMessageOps.set(handleId, promise);
            try {
              await promise;
            } finally{
              finishMessageOps.delete(handleId);
            }
          }
        })();
      }
      for (const { 1: promise } of new SafeMapIterator(finishMessageOps)){
        await promise;
      }
      finishMessageOps.clear();
    }
    watch(keys, options = {
      __proto__: null
    }) {
      const raw = options.raw ?? false;
      const rid = op_kv_watch(this.#rid, keys);
      const lastEntries = ArrayFrom({
        length: keys.length
      });
      return new ReadableStream({
        async pull (controller) {
          while(true){
            let updates;
            try {
              updates = await op_kv_watch_next(rid);
            } catch (err) {
              core.tryClose(rid);
              controller.error(err);
              return;
            }
            if (updates === null) {
              core.tryClose(rid);
              controller.close();
              return;
            }
            let changed = false;
            for(let i = 0; i < keys.length; i++){
              if (updates[i] === "unchanged") {
                if (lastEntries[i] === undefined) {
                  throw new Error("'watch': invalid unchanged update (internal error)");
                }
                continue;
              }
              if (lastEntries[i] !== undefined && (updates[i]?.versionstamp ?? null) === lastEntries[i]?.versionstamp) {
                continue;
              }
              changed = true;
              if (updates[i] === null) {
                lastEntries[i] = {
                  key: ArrayPrototypeSlice(keys[i]),
                  value: null,
                  versionstamp: null
                };
              } else {
                lastEntries[i] = updates[i];
              }
            }
            if (!changed && !raw) continue; // no change
            const entries = ArrayPrototypeMap(lastEntries, (entry)=>entry.versionstamp === null ? {
                ...entry
              } : deserializeValue(entry));
            controller.enqueue(entries);
            return;
          }
        },
        cancel () {
          core.tryClose(rid);
        }
      });
    }
    close() {
      core.close(this.#rid);
      this.#isClosed = true;
    }
    [SymbolDispose]() {
      core.tryClose(this.#rid);
    }
  }
  class AtomicOperation {
    #rid;
    #checks = [];
    #mutations = [];
    #enqueues = [];
    constructor(rid){
      this.#rid = rid;
    }
    check(...checks) {
      for(let i = 0; i < checks.length; ++i){
        const check = checks[i];
        ArrayPrototypePush(this.#checks, [
          check.key,
          check.versionstamp
        ]);
      }
      return this;
    }
    mutate(...mutations) {
      for(let i = 0; i < mutations.length; ++i){
        const mutation = mutations[i];
        const key = mutation.key;
        let type;
        let value;
        let expireIn = undefined;
        switch(mutation.type){
          case "delete":
            type = "delete";
            if (mutation.value) {
              throw new TypeError("Invalid mutation 'delete' with value");
            }
            break;
          case "set":
            if (typeof mutation.expireIn === "number") {
              expireIn = mutation.expireIn;
            }
            validateExpireIn(expireIn);
          /* falls through */ case "sum":
          case "min":
          case "max":
            type = mutation.type;
            if (!ObjectHasOwn(mutation, "value")) {
              throw new TypeError(`Invalid mutation '${type}' without value`);
            }
            value = serializeValue(mutation.value);
            break;
          default:
            throw new TypeError("Invalid mutation type");
        }
        ArrayPrototypePush(this.#mutations, [
          key,
          type,
          value,
          expireIn
        ]);
      }
      return this;
    }
    sum(key, n) {
      ArrayPrototypePush(this.#mutations, [
        key,
        "sum",
        serializeValue(new KvU64(n)),
        undefined
      ]);
      return this;
    }
    min(key, n) {
      ArrayPrototypePush(this.#mutations, [
        key,
        "min",
        serializeValue(new KvU64(n)),
        undefined
      ]);
      return this;
    }
    max(key, n) {
      ArrayPrototypePush(this.#mutations, [
        key,
        "max",
        serializeValue(new KvU64(n)),
        undefined
      ]);
      return this;
    }
    set(key, value, options) {
      validateExpireIn(options?.expireIn);
      ArrayPrototypePush(this.#mutations, [
        key,
        "set",
        serializeValue(value),
        options?.expireIn
      ]);
      return this;
    }
    delete(key) {
      ArrayPrototypePush(this.#mutations, [
        key,
        "delete",
        null,
        undefined
      ]);
      return this;
    }
    enqueue(message, opts) {
      if (opts?.delay !== undefined) {
        validateQueueDelay(opts?.delay);
      }
      if (opts?.backoffSchedule !== undefined) {
        validateBackoffSchedule(opts?.backoffSchedule);
      }
      ArrayPrototypePush(this.#enqueues, [
        core.serialize(message, {
          forStorage: true
        }),
        opts?.delay ?? 0,
        opts?.keysIfUndelivered ?? [],
        opts?.backoffSchedule ?? null
      ]);
      return this;
    }
    async commit() {
      const versionstamp = await doAtomicWriteInPlace(this.#rid, this.#checks, this.#mutations, this.#enqueues);
      if (versionstamp === null) return {
        ok: false
      };
      return {
        ok: true,
        versionstamp
      };
    }
    then() {
      throw new TypeError("'Deno.AtomicOperation' is not a promise: did you forget to call 'commit()'");
    }
    [SymbolFor("Deno.privateCustomInspect")](inspect, inspectOptions) {
      const operations = [];
      // Format checks
      for(let i = 0; i < this.#checks.length; ++i){
        const check = this.#checks[i];
        const key = check[0];
        const versionstamp = check[1];
        const keyStr = inspect(key, inspectOptions);
        const versionstampStr = versionstamp === null ? "null" : `"${versionstamp}"`;
        ArrayPrototypePush(operations, `  check({ key: ${keyStr}, versionstamp: ${versionstampStr} })`);
      }
      // Format mutations
      for(let i = 0; i < this.#mutations.length; ++i){
        const mutation = this.#mutations[i];
        const key = mutation[0];
        const type = mutation[1];
        const rawValue = mutation[2];
        const expireIn = mutation[3];
        const keyStr = inspect(key, inspectOptions);
        if (type === "delete") {
          ArrayPrototypePush(operations, `  delete(${keyStr})`);
        } else {
          // Deserialize value for display
          let value;
          try {
            if (rawValue === null) {
              value = null;
            } else {
              switch(rawValue.kind){
                case "v8":
                  value = core.deserialize(rawValue.value, {
                    forStorage: true,
                    deserializers: cloneableDeserializers
                  });
                  break;
                case "bytes":
                  value = rawValue.value;
                  break;
                case "u64":
                  value = new KvU64(rawValue.value);
                  break;
                default:
                  value = rawValue;
              }
            }
          } catch  {
            // If deserialization fails, show the raw value structure
            value = `[${rawValue?.kind || "unknown"} value]`;
          }
          const valueStr = inspect(value, inspectOptions);
          if (type === "set" && expireIn !== undefined) {
            ArrayPrototypePush(operations, `  set(${keyStr}, ${valueStr}, { expireIn: ${expireIn} })`);
          } else {
            ArrayPrototypePush(operations, `  ${type}(${keyStr}, ${valueStr})`);
          }
        }
      }
      // Format enqueues
      for(let i = 0; i < this.#enqueues.length; ++i){
        const enqueue = this.#enqueues[i];
        const serializedMessage = enqueue[0];
        const delay = enqueue[1];
        const keysIfUndelivered = enqueue[2];
        const backoffSchedule = enqueue[3];
        // Deserialize message for display
        let message;
        try {
          message = core.deserialize(serializedMessage, {
            forStorage: true,
            deserializers: cloneableDeserializers
          });
        } catch  {
          message = "[serialized message]";
        }
        const messageStr = inspect(message, inspectOptions);
        if (delay === 0 && keysIfUndelivered.length === 0 && backoffSchedule === null) {
          ArrayPrototypePush(operations, `  enqueue(${messageStr})`);
        } else {
          const options = [];
          if (delay !== 0) ArrayPrototypePush(options, `delay: ${delay}`);
          if (keysIfUndelivered.length > 0) {
            const keysStr = inspect(keysIfUndelivered, inspectOptions);
            ArrayPrototypePush(options, `keysIfUndelivered: ${keysStr}`);
          }
          if (backoffSchedule !== null) {
            const scheduleStr = inspect(backoffSchedule, inspectOptions);
            ArrayPrototypePush(options, `backoffSchedule: ${scheduleStr}`);
          }
          ArrayPrototypePush(operations, `  enqueue(${messageStr}, { ${ArrayPrototypeJoin(options, ", ")} })`);
        }
      }
      if (operations.length === 0) {
        return "AtomicOperation (empty)";
      }
      return `AtomicOperation\n${ArrayPrototypeJoin(operations, "\n")}`;
    }
  }
  const MIN_U64 = BigInt("0");
  const MAX_U64 = BigInt("0xffffffffffffffff");
  class KvU64 {
    value;
    constructor(value){
      if (typeof value !== "bigint") {
        throw new TypeError(`Value must be a bigint: received ${typeof value}`);
      }
      if (value < MIN_U64) {
        throw new RangeError(`Value must be a positive bigint: received ${value}`);
      }
      if (value > MAX_U64) {
        throw new RangeError("Value must fit in a 64-bit unsigned integer");
      }
      this.value = value;
      ObjectFreeze(this);
    }
    valueOf() {
      return this.value;
    }
    toString() {
      return BigIntPrototypeToString(this.value);
    }
    get [SymbolToStringTag]() {
      return "Deno.KvU64";
    }
    [SymbolFor("Deno.privateCustomInspect")](inspect, inspectOptions) {
      return StringPrototypeReplace(inspect(Object(this.value), inspectOptions), "BigInt", "Deno.KvU64");
    }
  }
  function deserializeValue(entry) {
    const { kind, value } = entry.value;
    switch(kind){
      case "v8":
        return {
          ...entry,
          value: core.deserialize(value, {
            forStorage: true,
            deserializers: cloneableDeserializers
          })
        };
      case "bytes":
        return {
          ...entry,
          value
        };
      case "u64":
        return {
          ...entry,
          value: new KvU64(value)
        };
      default:
        throw new TypeError("Invalid value type");
    }
  }
  function serializeValue(value) {
    if (TypedArrayPrototypeGetSymbolToStringTag(value) === "Uint8Array") {
      return {
        kind: "bytes",
        value
      };
    } else if (ObjectPrototypeIsPrototypeOf(KvU64.prototype, value)) {
      return {
        kind: "u64",
        // deno-lint-ignore deno-internal/prefer-primordials
        value: value.valueOf()
      };
    } else {
      return {
        kind: "v8",
        value: core.serialize(value, {
          forStorage: true
        })
      };
    }
  }
  // This gets the %AsyncIteratorPrototype% object (which exists but is not a
  // global). We extend the KvListIterator iterator from, so that we immediately
  // support async iterator helpers once they land. The %AsyncIterator% does not
  // yet actually exist however, so right now the AsyncIterator binding refers to
  // %Object%. I know.
  // Once AsyncIterator is a global, we can just use it (from primordials), rather
  // than doing this here.
  const AsyncIteratorPrototype = ObjectGetPrototypeOf(AsyncGeneratorPrototype);
  const AsyncIterator = AsyncIteratorPrototype.constructor;
  class KvListIterator extends AsyncIterator {
    #selector;
    #entries = null;
    #cursorGen = null;
    #done = false;
    #lastBatch = false;
    #pullBatch;
    #limit;
    #count = 0;
    #reverse;
    #batchSize;
    #consistency;
    constructor({ limit, selector, cursor, reverse, consistency, batchSize, pullBatch }){
      super();
      let prefix;
      let start;
      let end;
      if (ObjectHasOwn(selector, "prefix") && selector.prefix !== undefined) {
        prefix = ObjectFreeze(ArrayPrototypeSlice(selector.prefix));
      }
      if (ObjectHasOwn(selector, "start") && selector.start !== undefined) {
        start = ObjectFreeze(ArrayPrototypeSlice(selector.start));
      }
      if (ObjectHasOwn(selector, "end") && selector.end !== undefined) {
        end = ObjectFreeze(ArrayPrototypeSlice(selector.end));
      }
      if (prefix) {
        if (start && end) {
          throw new TypeError("Selector can not specify both 'start' and 'end' key when specifying 'prefix'");
        }
        if (start) {
          this.#selector = {
            prefix,
            start
          };
        } else if (end) {
          this.#selector = {
            prefix,
            end
          };
        } else {
          this.#selector = {
            prefix
          };
        }
      } else {
        if (start && end) {
          this.#selector = {
            start,
            end
          };
        } else {
          throw new TypeError("Selector must specify either 'prefix' or both 'start' and 'end' key");
        }
      }
      ObjectFreeze(this.#selector);
      this.#pullBatch = pullBatch;
      this.#limit = limit;
      this.#reverse = reverse;
      this.#consistency = consistency;
      this.#batchSize = batchSize;
      this.#cursorGen = cursor ? ()=>cursor : null;
    }
    get cursor() {
      if (this.#cursorGen === null) {
        throw new Error("Cannot get cursor before first iteration");
      }
      return this.#cursorGen();
    }
    async next() {
      // Fused or limit exceeded
      if (this.#done || this.#limit !== undefined && this.#count >= this.#limit) {
        return {
          done: true,
          value: undefined
        };
      }
      // Attempt to fill the buffer
      if (!this.#entries?.length && !this.#lastBatch) {
        const batch = await this.#pullBatch(this.#selector, this.#cursorGen ? this.#cursorGen() : undefined, this.#reverse, this.#consistency);
        // Reverse the batch so we can pop from the end
        ArrayPrototypeReverse(batch);
        this.#entries = batch;
        // Last batch, do not attempt to pull more
        if (batch.length < this.#batchSize) {
          this.#lastBatch = true;
        }
      }
      const entry = this.#entries?.pop();
      if (!entry) {
        this.#done = true;
        this.#cursorGen = ()=>"";
        return {
          done: true,
          value: undefined
        };
      }
      this.#cursorGen = ()=>{
        const selector = this.#selector;
        return encodeCursor([
          ObjectHasOwn(selector, "prefix") ? selector.prefix : null,
          ObjectHasOwn(selector, "start") ? selector.start : null,
          ObjectHasOwn(selector, "end") ? selector.end : null
        ], entry.key);
      };
      this.#count++;
      return {
        done: false,
        value: entry
      };
    }
    [SymbolAsyncIterator]() {
      return this;
    }
  }
  async function doAtomicWriteInPlace(rid, checks, mutations, enqueues) {
    for(let i = 0; i < mutations.length; ++i){
      const mutation = mutations[i];
      const key = mutation[0];
      if (key.length && mutation[1] === "set" && key[key.length - 1] === commitVersionstampSymbol) {
        mutation[0] = ArrayPrototypeSlice(key, 0, key.length - 1);
        mutation[1] = "setSuffixVersionstampedKey";
      }
    }
    return await op_kv_atomic_write(rid, checks, mutations, enqueues);
  }
  return {
    AtomicOperation,
    Kv,
    KvListIterator,
    KvU64,
    openKv
  };
})());