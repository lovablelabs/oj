"use strict"; return ((function() {
  const { core, internals, primordials } = __bootstrap;
  const { op_otel_collect_isolate_metrics, op_otel_enable_isolate_metrics, op_otel_log, op_otel_log_foreign, op_otel_metric_attribute3, op_otel_metric_observable_record0, op_otel_metric_observable_record1, op_otel_metric_observable_record2, op_otel_metric_observable_record3, op_otel_metric_observation_done, op_otel_metric_record0, op_otel_metric_record1, op_otel_metric_record2, op_otel_metric_record3, op_otel_metric_wait_to_observe, op_otel_span_add_link, op_otel_span_attribute1, op_otel_span_attribute2, op_otel_span_attribute3, op_otel_span_update_name, OtelMeter, OtelTracer } = core.ops;
  const { Console } = core.loadExtScript("ext:deno_web/01_console.js");
  const { ArrayFrom, ArrayIsArray, ArrayPrototypeConcat, ArrayPrototypeFilter, ArrayPrototypeForEach, ArrayPrototypeJoin, ArrayPrototypeMap, ArrayPrototypePush, ArrayPrototypeReduce, ArrayPrototypeReverse, ArrayPrototypeShift, ArrayPrototypeSlice, DatePrototype, DatePrototypeGetTime, decodeURIComponent, encodeURIComponent, Error, MapPrototypeEntries, MapPrototypeKeys, Number, NumberParseInt, NumberPrototypeToString, ObjectAssign, ObjectDefineProperty, ObjectEntries, ObjectKeys, ObjectPrototypeIsPrototypeOf, ObjectValues, ReflectApply, SafeArrayIterator, SafeMap, SafeMapIterator, SafePromiseAll, SafeRegExp, SafeSet, SafeWeakSet, StringPrototypeIndexOf, StringPrototypeSlice, StringPrototypeSplit, StringPrototypeSubstring, StringPrototypeTrim, SymbolFor, TypeError } = primordials;
  const { AsyncVariable, getAsyncContext, setAsyncContext } = core;
  let TRACING_ENABLED = false;
  let METRICS_ENABLED = false;
  let PROPAGATORS = [];
  let ISOLATE_METRICS = false;
  // Note: These start at 0 in the JS library,
  // but start at 1 when serialized with JSON.
  let SpanKind = /*#__PURE__*/ function(SpanKind) {
    SpanKind[SpanKind["INTERNAL"] = 0] = "INTERNAL";
    SpanKind[SpanKind["SERVER"] = 1] = "SERVER";
    SpanKind[SpanKind["CLIENT"] = 2] = "CLIENT";
    SpanKind[SpanKind["PRODUCER"] = 3] = "PRODUCER";
    SpanKind[SpanKind["CONSUMER"] = 4] = "CONSUMER";
    return SpanKind;
  }({});
  let SpanStatusCode = /*#__PURE__*/ function(SpanStatusCode) {
    SpanStatusCode[SpanStatusCode["UNSET"] = 0] = "UNSET";
    SpanStatusCode[SpanStatusCode["OK"] = 1] = "OK";
    SpanStatusCode[SpanStatusCode["ERROR"] = 2] = "ERROR";
    return SpanStatusCode;
  }({});
  function hrToMs(hr) {
    return hr[0] * 1e3 + hr[1] / 1e6;
  }
  function isTimeInput(input) {
    return typeof input === "number" || input && (ArrayIsArray(input) || isDate(input));
  }
  function timeInputToMs(input) {
    if (input === undefined) return;
    if (ArrayIsArray(input)) {
      return hrToMs(input);
    } else if (isDate(input)) {
      return DatePrototypeGetTime(input);
    }
    return input;
  }
  function countAttributes(attributes) {
    return attributes ? ObjectKeys(attributes).length : 0;
  }
  const currentSnapshot = getAsyncContext;
  const restoreSnapshot = setAsyncContext;
  // A `unique symbol` type requires a direct `Symbol.for()` call, which is what
  // makes `SpanSnapshot` below narrowable. This runs during bootstrap, before any
  // user code, so reaching for the global here is safe.
  // deno-lint-ignore deno-internal/prefer-primordials
  const DID_NOT_ENTER = Symbol.for("Deno.telemetry.didNotEnterSpan");
  function enterSpan(span, context) {
    if (!span.isRecording()) return DID_NOT_ENTER;
    const snapshot = currentSnapshot();
    context = (context ?? CURRENT.get() ?? ROOT_CONTEXT).setValue(SPAN_KEY, span);
    CURRENT.enter(context);
    return snapshot;
  }
  function exitSpan(snapshot) {
    if (snapshot !== DID_NOT_ENTER) restoreSnapshot(snapshot);
  }
  function isDate(value) {
    return ObjectPrototypeIsPrototypeOf(DatePrototype, value);
  }
  let SpanAttributesLocation = /*#__PURE__*/ function(SpanAttributesLocation) {
    SpanAttributesLocation[SpanAttributesLocation["SELF"] = 0] = "SELF";
    SpanAttributesLocation[SpanAttributesLocation["EVENT"] = 1] = "EVENT";
    SpanAttributesLocation[SpanAttributesLocation["LINK"] = 2] = "LINK";
    return SpanAttributesLocation;
  }({});
  function spanAddAttributes(span, attributesLocation, attributesTarget, attributes) {
    const attributeKvs = ObjectEntries(attributes);
    let i = 0;
    while(i < attributeKvs.length){
      if (i + 2 < attributeKvs.length) {
        op_otel_span_attribute3(span, attributesLocation, attributesTarget, attributeKvs[i][0], attributeKvs[i][1], attributeKvs[i + 1][0], attributeKvs[i + 1][1], attributeKvs[i + 2][0], attributeKvs[i + 2][1]);
        i += 3;
      } else if (i + 1 < attributeKvs.length) {
        op_otel_span_attribute2(span, attributesLocation, attributesTarget, attributeKvs[i][0], attributeKvs[i][1], attributeKvs[i + 1][0], attributeKvs[i + 1][1]);
        i += 2;
      } else {
        op_otel_span_attribute1(span, attributesLocation, attributesTarget, attributeKvs[i][0], attributeKvs[i][1]);
        i += 1;
      }
    }
  }
  class TracerProvider {
    constructor(){
      throw new TypeError("TracerProvider can not be constructed");
    }
    static getTracer(name, version, options) {
      const tracer = new OtelTracer(name, version, options?.schemaUrl);
      return new Tracer(tracer);
    }
  }
  class Tracer {
    #tracer;
    constructor(tracer){
      this.#tracer = tracer;
    }
    startActiveSpan(name, optionsOrFn, fnOrContext, maybeFn) {
      let options;
      let context;
      let fn;
      if (typeof optionsOrFn === "function") {
        options = undefined;
        fn = optionsOrFn;
      } else if (typeof fnOrContext === "function") {
        options = optionsOrFn;
        fn = fnOrContext;
      } else if (typeof maybeFn === "function") {
        options = optionsOrFn;
        context = fnOrContext;
        fn = maybeFn;
      } else {
        throw new Error("startActiveSpan requires a function argument");
      }
      if (options?.root) {
        context = ROOT_CONTEXT;
      } else {
        context = context ?? CURRENT.get() ?? ROOT_CONTEXT;
      }
      const span = this.startSpan(name, options, context);
      const ctx = CURRENT.enter(context.setValue(SPAN_KEY, span));
      try {
        return ReflectApply(fn, undefined, [
          span
        ]);
      } finally{
        setAsyncContext(ctx);
      }
    }
    startSpan(name, options, context) {
      if (options?.root) {
        context = undefined;
      } else {
        context = context ?? CURRENT.get();
      }
      const startTime = timeInputToMs(options?.startTime);
      const parentSpan = context?.getValue(SPAN_KEY);
      const attributesCount = countAttributes(options?.attributes);
      const parentOtelSpan = parentSpan !== undefined ? getOtelSpan(parentSpan) ?? undefined : undefined;
      let otelSpan;
      if (parentOtelSpan || !parentSpan) {
        otelSpan = this.#tracer.startSpan(parentOtelSpan, name, options?.kind ?? 0, startTime, attributesCount);
      } else {
        const spanContext = parentSpan.spanContext();
        otelSpan = this.#tracer.startSpanForeign(spanContext.traceId, spanContext.spanId, spanContext.traceFlags ?? 0, name, options?.kind ?? 0, startTime, attributesCount);
      }
      const span = new Span(otelSpan);
      if (options?.links) span.addLinks(options?.links);
      if (options?.attributes) span.setAttributes(options?.attributes);
      return span;
    }
  }
  const SPAN_KEY = SymbolFor("OpenTelemetry Context Key SPAN");
  let getOtelSpan;
  class Span {
    #otelSpan;
    #spanContext;
    static{
      getOtelSpan = (span)=>#otelSpan in span ? span.#otelSpan : undefined;
    }
    constructor(otelSpan){
      this.#otelSpan = otelSpan;
    }
    spanContext() {
      if (!this.#spanContext) {
        if (this.#otelSpan) {
          this.#spanContext = this.#otelSpan.spanContext();
        } else {
          this.#spanContext = {
            traceId: "00000000000000000000000000000000",
            spanId: "0000000000000000",
            traceFlags: 0
          };
        }
      }
      return this.#spanContext;
    }
    addEvent(name, attributesOrStartTime, startTime) {
      if (!this.#otelSpan) return this;
      let attributes;
      if (isTimeInput(attributesOrStartTime)) {
        startTime = attributesOrStartTime;
      } else {
        attributes = attributesOrStartTime;
      }
      const startTimeMs = timeInputToMs(startTime);
      const attributesTarget = this.#otelSpan.addEvent(name, startTimeMs ?? NaN);
      if (attributes && attributesTarget !== 0) {
        spanAddAttributes(this.#otelSpan, SpanAttributesLocation.EVENT, attributesTarget, attributes);
      }
      return this;
    }
    addLink(link) {
      if (!this.#otelSpan) return this;
      const attributesTarget = op_otel_span_add_link(this.#otelSpan, link.context.traceId, link.context.spanId, link.context.traceFlags, link.context.isRemote ?? false, link.droppedAttributesCount ?? 0);
      if (link.attributes && attributesTarget !== 0) {
        spanAddAttributes(this.#otelSpan, SpanAttributesLocation.LINK, attributesTarget, link.attributes);
      }
      return this;
    }
    addLinks(links) {
      for(let i = 0; i < links.length; i++){
        this.addLink(links[i]);
      }
      return this;
    }
    end(endTime) {
      this.#otelSpan?.end(timeInputToMs(endTime) || NaN);
    }
    isRecording() {
      return this.#otelSpan !== undefined;
    }
    recordException(exception, time) {
      if (typeof exception === "string") {
        this.addEvent("exception", {
          "exception.message": exception
        }, time);
        return;
      }
      const attributes = {};
      if (exception.code) {
        if (typeof exception.code === "number") {
          attributes["exception.type"] = NumberPrototypeToString(exception.code);
        } else {
          attributes["exception.type"] = exception.code;
        }
      } else if (exception.name) {
        attributes["exception.type"] = exception.name;
      }
      if (exception.message) {
        attributes["exception.message"] = exception.message;
      }
      if (exception.stack) {
        attributes["exception.stacktrace"] = exception.stack;
      }
      this.addEvent("exception", attributes, time);
    }
    setAttribute(key, value) {
      if (!this.#otelSpan) return this;
      op_otel_span_attribute1(this.#otelSpan, SpanAttributesLocation.SELF, 0, key, value);
      return this;
    }
    setAttributes(attributes) {
      if (!this.#otelSpan) return this;
      spanAddAttributes(this.#otelSpan, SpanAttributesLocation.SELF, 0, attributes);
      return this;
    }
    setStatus(status) {
      this.#otelSpan?.setStatus(status.code, status.message ?? "");
      return this;
    }
    updateName(name) {
      if (!this.#otelSpan) return this;
      op_otel_span_update_name(this.#otelSpan, name);
      return this;
    }
  }
  const CURRENT = new AsyncVariable();
  class Context {
    // @ts-ignore __proto__ is not supported in TypeScript
    #data = {
      __proto__: null
    };
    constructor(data){
      // @ts-ignore __proto__ is not supported in TypeScript
      this.#data = {
        __proto__: null,
        ...data
      };
    }
    getValue(key) {
      return this.#data[key];
    }
    setValue(key, value) {
      const c = new Context(this.#data);
      c.#data[key] = value;
      return c;
    }
    deleteValue(key) {
      const c = new Context(this.#data);
      delete c.#data[key];
      return c;
    }
  }
  // TODO(lucacasonato): @opentelemetry/api defines it's own ROOT_CONTEXT
  const ROOT_CONTEXT = new Context();
  // Context manager for opentelemetry js library
  class ContextManager {
    constructor(){
      throw new TypeError("ContextManager can not be constructed");
    }
    static active() {
      return CURRENT.get() ?? ROOT_CONTEXT;
    }
    static with(context, fn, thisArg, ...args) {
      const ctx = CURRENT.enter(context);
      try {
        return ReflectApply(fn, thisArg, args);
      } finally{
        setAsyncContext(ctx);
      }
    }
    // deno-lint-ignore no-explicit-any
    static bind(context, target) {
      return (...args)=>{
        const ctx = CURRENT.enter(context);
        try {
          return ReflectApply(target, this, args);
        } finally{
          setAsyncContext(ctx);
        }
      };
    }
    static enable() {
      return this;
    }
    static disable() {
      return this;
    }
  }
  let ValueType = /*#__PURE__*/ function(ValueType) {
    ValueType[ValueType["INT"] = 0] = "INT";
    ValueType[ValueType["DOUBLE"] = 1] = "DOUBLE";
    return ValueType;
  }({});
  class MeterProvider {
    constructor(){
      throw new TypeError("MeterProvider can not be constructed");
    }
    static getMeter(name, version, options) {
      const meter = new OtelMeter(name, version, options?.schemaUrl);
      return new Meter(meter);
    }
  }
  let batchResultHasObservables;
  class BatchObservableResult {
    #observables;
    constructor(observables){
      this.#observables = observables;
    }
    static{
      batchResultHasObservables = (cb, observables)=>{
        for (const observable of new SafeArrayIterator(observables)){
          if (!cb.#observables.has(observable)) return false;
        }
        return true;
      };
    }
    observe(metric, value, attributes) {
      if (!this.#observables.has(metric)) return;
      getObservableResult(metric).observe(value, attributes);
    }
  }
  const BATCH_CALLBACKS = new SafeMap();
  const INDIVIDUAL_CALLBACKS = new SafeMap();
  class Meter {
    #meter;
    constructor(meter){
      this.#meter = meter;
    }
    createCounter(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) return new Counter(null, false);
      const instrument = this.#meter.createCounter(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit);
      return new Counter(instrument, false);
    }
    createUpDownCounter(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) return new Counter(null, true);
      const instrument = this.#meter.createUpDownCounter(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit);
      return new Counter(instrument, true);
    }
    createGauge(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) return new Gauge(null);
      const instrument = this.#meter.createGauge(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit);
      return new Gauge(instrument);
    }
    createHistogram(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) return new Histogram(null);
      const instrument = this.#meter.createHistogram(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit, options?.advice?.explicitBucketBoundaries);
      return new Histogram(instrument);
    }
    createObservableCounter(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) new Observable(new ObservableResult(null, true));
      const instrument = this.#meter.createObservableCounter(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit);
      return new Observable(new ObservableResult(instrument, true));
    }
    createObservableUpDownCounter(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) new Observable(new ObservableResult(null, false));
      const instrument = this.#meter.createObservableUpDownCounter(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit);
      return new Observable(new ObservableResult(instrument, false));
    }
    createObservableGauge(name, options) {
      if (options?.valueType !== undefined && options?.valueType !== 1) {
        throw new Error("Only valueType: DOUBLE is supported");
      }
      if (!METRICS_ENABLED) new Observable(new ObservableResult(null, false));
      const instrument = this.#meter.createObservableGauge(name, // deno-lint-ignore deno-internal/prefer-primordials
      options?.description, options?.unit);
      return new Observable(new ObservableResult(instrument, false));
    }
    addBatchObservableCallback(callback, observables) {
      if (!METRICS_ENABLED) return;
      const result = new BatchObservableResult(new SafeWeakSet(observables));
      startObserving();
      BATCH_CALLBACKS.set(callback, result);
    }
    removeBatchObservableCallback(callback, observables) {
      if (!METRICS_ENABLED) return;
      const result = BATCH_CALLBACKS.get(callback);
      if (result && batchResultHasObservables(result, observables)) {
        BATCH_CALLBACKS.delete(callback);
      }
    }
  }
  function record(instrument, value, attributes) {
    if (instrument === null) return;
    if (attributes === undefined) {
      op_otel_metric_record0(instrument, value);
    } else {
      const attrs = ObjectEntries(attributes);
      if (attrs.length === 0) {
        op_otel_metric_record0(instrument, value);
      }
      let i = 0;
      while(i < attrs.length){
        const remaining = attrs.length - i;
        if (remaining > 3) {
          op_otel_metric_attribute3(attrs.length, attrs[i][0], attrs[i][1], attrs[i + 1][0], attrs[i + 1][1], attrs[i + 2][0], attrs[i + 2][1]);
          i += 3;
        } else if (remaining === 3) {
          op_otel_metric_record3(instrument, value, attrs[i][0], attrs[i][1], attrs[i + 1][0], attrs[i + 1][1], attrs[i + 2][0], attrs[i + 2][1]);
          i += 3;
        } else if (remaining === 2) {
          op_otel_metric_record2(instrument, value, attrs[i][0], attrs[i][1], attrs[i + 1][0], attrs[i + 1][1]);
          i += 2;
        } else if (remaining === 1) {
          op_otel_metric_record1(instrument, value, attrs[i][0], attrs[i][1]);
          i += 1;
        }
      }
    }
  }
  function recordObservable(instrument, value, attributes) {
    if (instrument === null) return;
    if (attributes === undefined) {
      op_otel_metric_observable_record0(instrument, value);
    } else {
      const attrs = ObjectEntries(attributes);
      if (attrs.length === 0) {
        op_otel_metric_observable_record0(instrument, value);
      }
      let i = 0;
      while(i < attrs.length){
        const remaining = attrs.length - i;
        if (remaining > 3) {
          op_otel_metric_attribute3(attrs.length, attrs[i][0], attrs[i][1], attrs[i + 1][0], attrs[i + 1][1], attrs[i + 2][0], attrs[i + 2][1]);
          i += 3;
        } else if (remaining === 3) {
          op_otel_metric_observable_record3(instrument, value, attrs[i][0], attrs[i][1], attrs[i + 1][0], attrs[i + 1][1], attrs[i + 2][0], attrs[i + 2][1]);
          i += 3;
        } else if (remaining === 2) {
          op_otel_metric_observable_record2(instrument, value, attrs[i][0], attrs[i][1], attrs[i + 1][0], attrs[i + 1][1]);
          i += 2;
        } else if (remaining === 1) {
          op_otel_metric_observable_record1(instrument, value, attrs[i][0], attrs[i][1]);
          i += 1;
        }
      }
    }
  }
  class Counter {
    #instrument;
    #upDown;
    constructor(instrument, upDown){
      this.#instrument = instrument;
      this.#upDown = upDown;
    }
    add(value, attributes, _context) {
      if (value < 0 && !this.#upDown) {
        throw new Error("Counter can only be incremented");
      }
      record(this.#instrument, value, attributes);
    }
  }
  class Gauge {
    #instrument;
    constructor(instrument){
      this.#instrument = instrument;
    }
    record(value, attributes, _context) {
      record(this.#instrument, value, attributes);
    }
  }
  class Histogram {
    #instrument;
    constructor(instrument){
      this.#instrument = instrument;
    }
    record(value, attributes, _context) {
      record(this.#instrument, value, attributes);
    }
  }
  let getObservableResult;
  class Observable {
    #result;
    constructor(result){
      this.#result = result;
    }
    static{
      getObservableResult = (observable)=>observable.#result;
    }
    addCallback(callback) {
      const res = INDIVIDUAL_CALLBACKS.get(this);
      if (res) res.add(callback);
      else INDIVIDUAL_CALLBACKS.set(this, new SafeSet([
        callback
      ]));
      startObserving();
    }
    removeCallback(callback) {
      const res = INDIVIDUAL_CALLBACKS.get(this);
      if (res) res.delete(callback);
      if (res?.size === 0) INDIVIDUAL_CALLBACKS.delete(this);
    }
  }
  class ObservableResult {
    #instrument;
    #isRegularCounter;
    constructor(instrument, isRegularCounter){
      this.#instrument = instrument;
      this.#isRegularCounter = isRegularCounter;
    }
    observe(value, attributes) {
      if (this.#isRegularCounter) {
        if (value < 0) {
          throw new Error("Observable counters can only be incremented");
        }
      }
      recordObservable(this.#instrument, value, attributes);
    }
  }
  async function observe() {
    if (ISOLATE_METRICS) {
      op_otel_collect_isolate_metrics();
    }
    const promises = [];
    // Primordials are not needed, because this is a SafeMap.
    // deno-lint-ignore deno-internal/prefer-primordials
    for (const { 0: observable, 1: callbacks } of INDIVIDUAL_CALLBACKS){
      const result = getObservableResult(observable);
      // Primordials are not needed, because this is a SafeSet.
      // deno-lint-ignore deno-internal/prefer-primordials
      for (const callback of callbacks){
        // PromiseTry is not in primordials?
        // deno-lint-ignore deno-internal/prefer-primordials
        ArrayPrototypePush(promises, Promise.try(callback, result));
      }
    }
    // Primordials are not needed, because this is a SafeMap.
    // deno-lint-ignore deno-internal/prefer-primordials
    for (const { 0: callback, 1: result } of BATCH_CALLBACKS){
      // PromiseTry is not in primordials?
      // deno-lint-ignore deno-internal/prefer-primordials
      ArrayPrototypePush(promises, Promise.try(callback, result));
    }
    await SafePromiseAll(promises);
  }
  let isObserving = false;
  function startObserving() {
    if (!isObserving) {
      isObserving = true;
      (async ()=>{
        while(true){
          const promise = op_otel_metric_wait_to_observe();
          core.unrefOpPromise(promise);
          const ok = await promise;
          if (!ok) break;
          await observe();
          op_otel_metric_observation_done();
        }
      })();
    }
  }
  const otelConsoleConfig = {
    ignore: 0,
    capture: 1,
    replace: 2
  };
  let pendingException;
  const OTEL_CONSOLE_METHODS = [
    "log",
    "debug",
    "info",
    "warn",
    "error"
  ];
  function wrapOtelConsoleMethods(otelConsole) {
    for(let i = 0; i < OTEL_CONSOLE_METHODS.length; i++){
      const method = OTEL_CONSOLE_METHODS[i];
      const orig = otelConsole[method];
      otelConsole[method] = (...args)=>{
        if (args.length >= 1 && core.isNativeError(args[0])) {
          pendingException = args[0];
        }
        return ReflectApply(orig, otelConsole, args);
      };
    }
  }
  function otelLog(message, level) {
    const exception = pendingException;
    pendingException = undefined;
    const excType = exception?.name ?? "";
    const excMessage = exception?.message ?? "";
    const excStacktrace = exception?.stack ?? "";
    const currentSpan = CURRENT.get()?.getValue(SPAN_KEY);
    const otelSpan = currentSpan !== undefined ? getOtelSpan(currentSpan) : undefined;
    if (otelSpan || currentSpan === undefined) {
      op_otel_log(message, level, otelSpan, excType, excMessage, excStacktrace);
    } else {
      const spanContext = currentSpan.spanContext();
      op_otel_log_foreign(message, level, spanContext.traceId, spanContext.spanId, spanContext.traceFlags, excType, excMessage, excStacktrace);
    }
  }
  /*
 * Copyright The OpenTelemetry Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */ const VERSION = "00";
  const VERSION_PART = "(?!ff)[\\da-f]{2}";
  const TRACE_ID_PART = "(?![0]{32})[\\da-f]{32}";
  const PARENT_ID_PART = "(?![0]{16})[\\da-f]{16}";
  const FLAGS_PART = "[\\da-f]{2}";
  const TRACE_PARENT_REGEX = new SafeRegExp(`^\\s?(${VERSION_PART})-(${TRACE_ID_PART})-(${PARENT_ID_PART})-(${FLAGS_PART})(-.*)?\\s?$`);
  const VALID_TRACEID_REGEX = new SafeRegExp("^([0-9a-f]{32})$", "i");
  const VALID_SPANID_REGEX = new SafeRegExp("^[0-9a-f]{16}$", "i");
  const MAX_TRACE_STATE_ITEMS = 32;
  const MAX_TRACE_STATE_LEN = 512;
  const LIST_MEMBERS_SEPARATOR = ",";
  const LIST_MEMBER_KEY_VALUE_SPLITTER = "=";
  const VALID_KEY_CHAR_RANGE = "[_0-9a-z-*/]";
  const VALID_KEY = `[a-z]${VALID_KEY_CHAR_RANGE}{0,255}`;
  const VALID_VENDOR_KEY = `[a-z0-9]${VALID_KEY_CHAR_RANGE}{0,240}@[a-z]${VALID_KEY_CHAR_RANGE}{0,13}`;
  const VALID_KEY_REGEX = new SafeRegExp(`^(?:${VALID_KEY}|${VALID_VENDOR_KEY})$`);
  const VALID_VALUE_BASE_REGEX = new SafeRegExp("^[ -~]{0,255}[!-~]$");
  const INVALID_VALUE_COMMA_EQUAL_REGEX = new SafeRegExp(",|=");
  const TRACE_PARENT_HEADER = "traceparent";
  const TRACE_STATE_HEADER = "tracestate";
  const INVALID_TRACEID = "00000000000000000000000000000000";
  const INVALID_SPANID = "0000000000000000";
  const INVALID_SPAN_CONTEXT = {
    traceId: INVALID_TRACEID,
    spanId: INVALID_SPANID,
    traceFlags: 0
  };
  const BAGGAGE_KEY_PAIR_SEPARATOR = "=";
  const BAGGAGE_PROPERTIES_SEPARATOR = ";";
  const BAGGAGE_ITEMS_SEPARATOR = ",";
  const BAGGAGE_HEADER = "baggage";
  const BAGGAGE_MAX_NAME_VALUE_PAIRS = 180;
  const BAGGAGE_MAX_PER_NAME_VALUE_PAIRS = 4096;
  const BAGGAGE_MAX_TOTAL_LENGTH = 8192;
  class NonRecordingSpan {
    _spanContext;
    constructor(_spanContext = INVALID_SPAN_CONTEXT){
      this._spanContext = _spanContext;
    }
    spanContext() {
      return this._spanContext;
    }
    setAttribute(_key, _value) {
      return this;
    }
    setAttributes(_attributes) {
      return this;
    }
    addEvent(_name, _attributes) {
      return this;
    }
    addLink(_link) {
      return this;
    }
    addLinks(_links) {
      return this;
    }
    setStatus(_status) {
      return this;
    }
    updateName(_name) {
      return this;
    }
    end(_endTime) {}
    isRecording() {
      return false;
    }
    // deno-lint-ignore no-explicit-any
    recordException(_exception, _time) {}
  }
  const otelPropagators = {
    traceContext: 0,
    baggage: 1,
    none: 2
  };
  function parseTraceParent(traceParent) {
    const match = TRACE_PARENT_REGEX.exec(traceParent);
    if (!match) return null;
    // According to the specification the implementation should be compatible
    // with future versions. If there are more parts, we only reject it if it's using version 00
    // See https://www.w3.org/TR/trace-context/#versioning-of-traceparent
    if (match[1] === "00" && match[5]) return null;
    return {
      traceId: match[2],
      spanId: match[3],
      traceFlags: NumberParseInt(match[4], 16)
    };
  }
  function isTracingSuppressed(context) {
    return context.getValue(SymbolFor("OpenTelemetry SDK Context Key SUPPRESS_TRACING")) === true;
  }
  function isValidTraceId(traceId) {
    return VALID_TRACEID_REGEX.test(traceId) && traceId !== INVALID_TRACEID;
  }
  function isValidSpanId(spanId) {
    return VALID_SPANID_REGEX.test(spanId) && spanId !== INVALID_SPANID;
  }
  function isSpanContextValid(spanContext) {
    return isValidTraceId(spanContext.traceId) && isValidSpanId(spanContext.spanId);
  }
  function validateKey(key) {
    return VALID_KEY_REGEX.test(key);
  }
  function validateValue(value) {
    return VALID_VALUE_BASE_REGEX.test(value) && !INVALID_VALUE_COMMA_EQUAL_REGEX.test(value);
  }
  class TraceStateClass {
    _internalState = new SafeMap();
    constructor(rawTraceState){
      if (rawTraceState) this._parse(rawTraceState);
    }
    set(key, value) {
      const traceState = this._clone();
      if (traceState._internalState.has(key)) {
        traceState._internalState.delete(key);
      }
      traceState._internalState.set(key, value);
      return traceState;
    }
    unset(key) {
      const traceState = this._clone();
      traceState._internalState.delete(key);
      return traceState;
    }
    get(key) {
      return this._internalState.get(key);
    }
    serialize() {
      return ArrayPrototypeJoin(ArrayPrototypeReduce(this._keys(), (agg, key)=>{
        ArrayPrototypePush(agg, key + LIST_MEMBER_KEY_VALUE_SPLITTER + this.get(key));
        return agg;
      }, []), LIST_MEMBERS_SEPARATOR);
    }
    _parse(rawTraceState) {
      if (rawTraceState.length > MAX_TRACE_STATE_LEN) return;
      this._internalState = ArrayPrototypeReduce(ArrayPrototypeReverse(StringPrototypeSplit(rawTraceState, LIST_MEMBERS_SEPARATOR)), (agg, part)=>{
        const listMember = StringPrototypeTrim(part); // Optional Whitespace (OWS) handling
        const i = StringPrototypeIndexOf(listMember, LIST_MEMBER_KEY_VALUE_SPLITTER);
        if (i !== -1) {
          const key = StringPrototypeSlice(listMember, 0, i);
          const value = StringPrototypeSlice(listMember, i + 1, part.length);
          if (validateKey(key) && validateValue(value)) {
            agg.set(key, value);
          }
        }
        return agg;
      }, new SafeMap());
      // Because of the reverse() requirement, trunc must be done after map is created
      if (this._internalState.size > MAX_TRACE_STATE_ITEMS) {
        this._internalState = new SafeMap(ArrayPrototypeSlice(ArrayPrototypeReverse(ArrayFrom(MapPrototypeEntries(this._internalState))), 0, MAX_TRACE_STATE_ITEMS));
      }
    }
    _keys() {
      return ArrayPrototypeReverse(ArrayFrom(MapPrototypeKeys(this._internalState)));
    }
    _clone() {
      const traceState = new TraceStateClass();
      traceState._internalState = new SafeMap(this._internalState);
      return traceState;
    }
  }
  class W3CTraceContextPropagator {
    inject(context, carrier, setter) {
      const spanContext = context.getValue(SPAN_KEY)?.spanContext();
      if (!spanContext || isTracingSuppressed(context) || !isSpanContextValid(spanContext)) {
        return;
      }
      const traceParent = `${VERSION}-${spanContext.traceId}-${spanContext.spanId}-0${NumberPrototypeToString(Number(spanContext.traceFlags || 0), 16)}`;
      setter.set(carrier, TRACE_PARENT_HEADER, traceParent);
      if (spanContext.traceState) {
        setter.set(carrier, TRACE_STATE_HEADER, spanContext.traceState.serialize());
      }
    }
    extract(context, carrier, getter) {
      const traceParentHeader = getter.get(carrier, TRACE_PARENT_HEADER);
      if (!traceParentHeader) return context;
      const traceParent = ArrayIsArray(traceParentHeader) ? traceParentHeader[0] : traceParentHeader;
      if (typeof traceParent !== "string") return context;
      const spanContext = parseTraceParent(traceParent);
      if (!spanContext) return context;
      spanContext.isRemote = true;
      const traceStateHeader = getter.get(carrier, TRACE_STATE_HEADER);
      if (traceStateHeader) {
        // If more than one `tracestate` header is found, we merge them into a
        // single header.
        const state = ArrayIsArray(traceStateHeader) ? ArrayPrototypeJoin(traceStateHeader, ",") : traceStateHeader;
        spanContext.traceState = new TraceStateClass(typeof state === "string" ? state : undefined);
      }
      return context.setValue(SPAN_KEY, new NonRecordingSpan(spanContext));
    }
    fields() {
      return [
        TRACE_PARENT_HEADER,
        TRACE_STATE_HEADER
      ];
    }
  }
  const baggageEntryMetadataSymbol = SymbolFor("BaggageEntryMetadata");
  function baggageEntryMetadataFromString(str) {
    if (typeof str !== "string") {
      str = "";
    }
    return {
      __TYPE__: baggageEntryMetadataSymbol,
      toString () {
        return str;
      }
    };
  }
  function serializeKeyPairs(keyPairs) {
    return ArrayPrototypeReduce(keyPairs, (hValue, current)=>{
      const value = `${hValue}${hValue !== "" ? BAGGAGE_ITEMS_SEPARATOR : ""}${current}`;
      return value.length > BAGGAGE_MAX_TOTAL_LENGTH ? hValue : value;
    }, "");
  }
  function getKeyPairs(baggage) {
    return ArrayPrototypeMap(baggage.getAllEntries(), (baggageEntry)=>{
      let entry = `${encodeURIComponent(baggageEntry[0])}=${encodeURIComponent(baggageEntry[1].value)}`;
      // include opaque metadata if provided
      // NOTE: we intentionally don't URI-encode the metadata - that responsibility falls on the metadata implementation
      if (baggageEntry[1].metadata !== undefined) {
        entry += BAGGAGE_PROPERTIES_SEPARATOR + // deno-lint-ignore deno-internal/prefer-primordials
        baggageEntry[1].metadata.toString();
      }
      return entry;
    });
  }
  function parsePairKeyValue(entry) {
    const valueProps = StringPrototypeSplit(entry, BAGGAGE_PROPERTIES_SEPARATOR);
    if (valueProps.length <= 0) return;
    const keyPairPart = ArrayPrototypeShift(valueProps);
    if (!keyPairPart) return;
    const separatorIndex = StringPrototypeIndexOf(keyPairPart, BAGGAGE_KEY_PAIR_SEPARATOR);
    if (separatorIndex <= 0) return;
    const key = decodeURIComponent(StringPrototypeTrim(StringPrototypeSubstring(keyPairPart, 0, separatorIndex)));
    const value = decodeURIComponent(StringPrototypeTrim(StringPrototypeSubstring(keyPairPart, separatorIndex + 1)));
    let metadata;
    if (valueProps.length > 0) {
      metadata = baggageEntryMetadataFromString(ArrayPrototypeJoin(valueProps, BAGGAGE_PROPERTIES_SEPARATOR));
    }
    return {
      key,
      value,
      metadata
    };
  }
  class BaggageImpl {
    #entries;
    constructor(entries){
      this.#entries = new SafeMap();
      // The `SafeMap` constructor that takes an iterable doesn't work for non Array iterables correctly.
      if (entries) {
        for (const { 0: key, 1: entry } of new SafeMapIterator(entries)){
          this.#entries.set(key, ObjectAssign({}, entry));
        }
      }
    }
    getEntry(key) {
      const entry = this.#entries.get(key);
      if (!entry) {
        return undefined;
      }
      return ObjectAssign({}, entry);
    }
    getAllEntries() {
      return ArrayPrototypeMap(ArrayFrom(MapPrototypeEntries(this.#entries)), (entry)=>[
          entry[0],
          entry[1]
        ]);
    }
    setEntry(key, entry) {
      const newBaggage = new BaggageImpl(this.#entries);
      newBaggage.#entries.set(key, entry);
      return newBaggage;
    }
    removeEntry(key) {
      const newBaggage = new BaggageImpl(this.#entries);
      newBaggage.#entries.delete(key);
      return newBaggage;
    }
    removeEntries(...keys) {
      const newBaggage = new BaggageImpl(this.#entries);
      for (const key of new SafeArrayIterator(keys)){
        newBaggage.#entries.delete(key);
      }
      return newBaggage;
    }
    clear() {
      return new BaggageImpl();
    }
  }
  const BAGGAGE_KEY = SymbolFor("OpenTelemetry Baggage Key");
  class W3CBaggagePropagator {
    inject(context, carrier, setter) {
      const baggage = context.getValue(BAGGAGE_KEY);
      if (!baggage || isTracingSuppressed(context)) return;
      const keyPairs = ArrayPrototypeSlice(ArrayPrototypeFilter(getKeyPairs(baggage), (pair)=>{
        return pair.length <= BAGGAGE_MAX_PER_NAME_VALUE_PAIRS;
      }), 0, BAGGAGE_MAX_NAME_VALUE_PAIRS);
      const headerValue = serializeKeyPairs(keyPairs);
      if (headerValue.length > 0) {
        setter.set(carrier, BAGGAGE_HEADER, headerValue);
      }
    }
    extract(context, carrier, getter) {
      const headerValue = getter.get(carrier, BAGGAGE_HEADER);
      const baggageString = ArrayIsArray(headerValue) ? ArrayPrototypeJoin(headerValue, BAGGAGE_ITEMS_SEPARATOR) : headerValue;
      if (!baggageString) return context;
      const baggage = {};
      const pairs = StringPrototypeSplit(baggageString, BAGGAGE_ITEMS_SEPARATOR);
      ArrayPrototypeForEach(pairs, (entry)=>{
        const keyPair = parsePairKeyValue(entry);
        if (keyPair) {
          const baggageEntry = {
            value: keyPair.value
          };
          if (keyPair.metadata) {
            baggageEntry.metadata = keyPair.metadata;
          }
          baggage[keyPair.key] = baggageEntry;
        }
      });
      if (ObjectEntries(baggage).length === 0) {
        return context;
      }
      return context.setValue(BAGGAGE_KEY, new BaggageImpl(new SafeMap(ObjectEntries(baggage))));
    }
    fields() {
      return [
        BAGGAGE_HEADER
      ];
    }
  }
  class CompositePropagator {
    #propagators;
    #fields;
    constructor(propagators){
      this.#propagators = propagators;
      this.#fields = ArrayFrom(new SafeSet(ArrayPrototypeReduce(ArrayPrototypeMap(this.#propagators, (p)=>p.fields()), (x, y)=>ArrayPrototypeConcat(x, y), [])));
    }
    inject(context, carrier, setter) {
      for (const propagator of new SafeArrayIterator(this.#propagators)){
        try {
          propagator.inject(context, carrier, setter);
        } catch (err) {
          // deno-lint-ignore no-console
          console.warn(`Failed to inject with ${propagator.constructor.name}.`, err);
        }
      }
    }
    extract(context, carrier, getter) {
      return ArrayPrototypeReduce(this.#propagators, (ctx, propagator)=>{
        try {
          return propagator.extract(ctx, carrier, getter);
        } catch (err) {
          // deno-lint-ignore no-console
          console.warn(`Failed to extract with ${propagator.constructor.name}.`, err);
        }
        return ctx;
      }, context);
    }
    fields() {
      return ArrayPrototypeSlice(this.#fields);
    }
  }
  let builtinTracerCache;
  function builtinTracer() {
    if (!builtinTracerCache) {
      builtinTracerCache = new Tracer(OtelTracer.builtin());
    }
    return builtinTracerCache;
  }
  function enableIsolateMetrics() {
    op_otel_enable_isolate_metrics();
    ISOLATE_METRICS = true;
    startObserving();
  }
  // We specify a very high version number, to allow any `@opentelemetry/api`
  // version to load this module. This does cause @opentelemetry/api to not be
  // able to register anything itself with the global registration methods.
  const OTEL_API_COMPAT_VERSION = "1.999.999";
  function bootstrap(config) {
    const { 0: tracingEnabled, 1: metricsEnabled, 2: consoleConfig, ...propagators } = config;
    TRACING_ENABLED = tracingEnabled === 1;
    METRICS_ENABLED = metricsEnabled === 1;
    PROPAGATORS = ArrayPrototypeMap(ArrayPrototypeFilter(ObjectValues(propagators), (propagator)=>propagator !== otelPropagators.none), (propagator)=>{
      switch(propagator){
        case otelPropagators.traceContext:
          return new W3CTraceContextPropagator();
        case otelPropagators.baggage:
          return new W3CBaggagePropagator();
      }
    });
    switch(consoleConfig){
      case otelConsoleConfig.capture:
        {
          const otelConsole = new Console(otelLog);
          wrapOtelConsoleMethods(otelConsole);
          core.wrapConsole(globalThis.console, otelConsole);
          break;
        }
      case otelConsoleConfig.replace:
        {
          const otelConsole = new Console(otelLog);
          wrapOtelConsoleMethods(otelConsole);
          ObjectDefineProperty(globalThis, "console", core.propNonEnumerable(otelConsole));
          break;
        }
      default:
        break;
    }
    if (TRACING_ENABLED || METRICS_ENABLED || PROPAGATORS.length > 0) {
      const otel = globalThis[SymbolFor("opentelemetry.js.api.1")] ??= {
        version: OTEL_API_COMPAT_VERSION
      };
      if (TRACING_ENABLED) {
        otel.trace = TracerProvider;
        otel.context = ContextManager;
      }
      if (METRICS_ENABLED) {
        otel.metrics = MeterProvider;
        enableIsolateMetrics();
      }
      if (PROPAGATORS.length > 0) {
        otel.propagation = new CompositePropagator(PROPAGATORS);
      }
    }
  }
  internals.__telemetry = {
    builtinTracer,
    ContextManager,
    DID_NOT_ENTER,
    enterSpan,
    exitSpan,
    get PROPAGATORS () {
      return PROPAGATORS;
    },
    restoreSnapshot,
    get TRACING_ENABLED () {
      return TRACING_ENABLED;
    }
  };
  const telemetry = {
    tracerProvider: TracerProvider,
    contextManager: ContextManager,
    meterProvider: MeterProvider
  };
  // Mutable state container: consumers destructure a reference to this
  // object, so property access at call-time always reflects the latest
  // values set by bootstrap().
  const otelState = {
    TRACING_ENABLED: false,
    METRICS_ENABLED: false,
    PROPAGATORS: [],
    getOtelSpan: undefined
  };
  // Keep module-level variables in sync with otelState for internal use
  // (existing code references the bare names).
  const _origBootstrap = bootstrap;
  function wrappedBootstrap(config) {
    _origBootstrap(config);
    otelState.TRACING_ENABLED = TRACING_ENABLED;
    otelState.METRICS_ENABLED = METRICS_ENABLED;
    otelState.PROPAGATORS = PROPAGATORS;
    otelState.getOtelSpan = getOtelSpan;
  }
  return {
    otelState,
    enterSpan,
    exitSpan,
    currentSnapshot,
    restoreSnapshot,
    SPAN_KEY,
    Span,
    ContextManager,
    baggageEntryMetadataFromString,
    W3CBaggagePropagator,
    CompositePropagator,
    builtinTracer,
    bootstrap: wrappedBootstrap,
    telemetry
  };
})());