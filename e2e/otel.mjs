// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// OJ_OTEL=1 exports over OTLP, configured by the standard OTEL_* variables: a
// failing module must arrive at the collector as a `compile_error` log record
// and in the `oj.diag.events` counter, and a request carrying a W3C
// traceparent must produce a server span that is that parent's child, plus a
// `http.server.request.duration` sample.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5397;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// A collector that keeps every OTLP/JSON body by signal.
const received = { logs: [], traces: [], metrics: [] };
const collector = http.createServer((req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    const signal = req.url.replace(/^\/v1\//, "");
    if (signal in received) received[signal].push(JSON.parse(body));
    res.writeHead(200, { "content-type": "application/json" });
    res.end("{}");
  });
});
await new Promise((resolve) => collector.listen(0, "127.0.0.1", resolve));
const endpoint = `http://127.0.0.1:${collector.address().port}`;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-otel-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "otel", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "ok.tsx"), `export const fine = 1;\n`);
fs.writeFileSync(path.join(app, "main.tsx"), `export const broken = {;\n`);

const proc = spawn(oj, ["dev", app, "--port", String(port)], {
  stdio: ["ignore", "ignore", "pipe"],
  env: {
    ...process.env,
    OJ_OTEL: "1",
    OTEL_EXPORTER_OTLP_ENDPOINT: endpoint,
    OTEL_EXPORTER_OTLP_PROTOCOL: "http/json",
    OTEL_SERVICE_NAME: "oj-e2e",
    OTEL_METRIC_EXPORT_INTERVAL: "500",
    OTEL_BLRP_SCHEDULE_DELAY: "200",
    OTEL_BSP_SCHEDULE_DELAY: "200",
  },
});
let stderr = "";
proc.stderr.on("data", (d) => (stderr += d));

const logRecords = () =>
  received.logs.flatMap((b) => b.resourceLogs.flatMap((r) => r.scopeLogs.flatMap((s) => s.logRecords)));
const spans = () =>
  received.traces.flatMap((b) => b.resourceSpans.flatMap((r) => r.scopeSpans.flatMap((s) => s.spans)));
const metrics = () =>
  received.metrics.flatMap((b) => b.resourceMetrics.flatMap((r) => r.scopeMetrics.flatMap((s) => s.metrics)));
const attr = (item, key) => {
  const v = item.attributes?.find((a) => a.key === key)?.value;
  return v && (v.stringValue ?? v.intValue);
};
const resourceName = (body, key) =>
  body[key][0].resource.attributes.find((a) => a.key === "service.name")?.value.stringValue;

const traceId = "4bf92f3577b34da6a3ce929d0e0e4736";
const parentId = "00f067aa0ba902b7";

let failed = false;
try {
  await waitUp(`http://localhost:${port}/ok.tsx`, { proc });

  const traced = await fetch(`http://localhost:${port}/ok.tsx`, {
    headers: { traceparent: `00-${traceId}-${parentId}-01` },
  });
  assert.equal(traced.status, 200);
  for (let i = 0; i < 3; i++) {
    const res = await fetch(`http://localhost:${port}/main.tsx`);
    assert.equal(res.status, 500, "the broken module is a 500");
  }

  const logged = await settles(() =>
    logRecords().some(
      (r) => attr(r, "oj.diag.kind") === "compile_error" && /main\.tsx/.test(r.body?.stringValue ?? ""),
    ),
  );
  assert.ok(logged, `a compile_error log record arrives, got: ${JSON.stringify(logRecords())}`);
  const record = logRecords().find((r) => attr(r, "oj.diag.kind") === "compile_error");
  assert.equal(record.severityText, "error");
  assert.equal(attr(record, "oj.diag.source"), "server");
  assert.equal(resourceName(received.logs[0], "resourceLogs"), "oj-e2e", "OTEL_SERVICE_NAME names the resource");

  const child = await settles(() => spans().some((s) => s.traceId === traceId && s.parentSpanId === parentId));
  assert.ok(child, `a span joins the incoming trace, got: ${JSON.stringify(spans())}`);
  const span = spans().find((s) => s.traceId === traceId);
  assert.equal(span.name, "GET");
  assert.equal(attr(span, "url.path"), "/ok.tsx");
  assert.equal(attr(span, "url.scheme"), "http");
  assert.equal(attr(span, "network.protocol.version"), "1.1");
  assert.equal(attr(span, "oj.route.class"), "module");
  assert.equal(Number(attr(span, "http.response.status_code")), 200);
  assert.ok(Number(span.endTimeUnixNano) >= Number(span.startTimeUnixNano), "the span ends after it starts");
  const failedSpan = spans().find((s) => attr(s, "url.path") === "/main.tsx");
  assert.ok(failedSpan, "requests without a traceparent start their own trace");
  assert.equal(failedSpan.status?.code, 2, "a 500 marks the span as an error");
  assert.equal(attr(failedSpan, "error.type"), "500");

  const counted = await settles(() => {
    const sums = metrics()
      .filter((m) => m.name === "oj.diag.events")
      .flatMap((m) => m.sum.dataPoints)
      .filter((p) => attr(p, "oj.diag.kind") === "compile_error");
    return sums.some((p) => Number(p.asInt) >= 3);
  });
  assert.ok(counted, `oj.diag.events counts every repeat, got: ${JSON.stringify(metrics())}`);
  const timed = metrics().some(
    (m) => m.name === "http.server.request.duration" && m.histogram.dataPoints.some((p) => Number(p.count) > 0),
  );
  assert.ok(timed, "request durations are recorded");

  // The embedded JS runtimes keep Deno's own instrumentation off: only oj's
  // scope reaches the collector.
  const scopes = new Set(
    [
      ...received.logs.flatMap((b) => b.resourceLogs.flatMap((r) => r.scopeLogs)),
      ...received.traces.flatMap((b) => b.resourceSpans.flatMap((r) => r.scopeSpans)),
      ...received.metrics.flatMap((b) => b.resourceMetrics.flatMap((r) => r.scopeMetrics)),
    ].map((s) => s.scope?.name),
  );
  assert.deepEqual([...scopes], ["oj"], "only oj's instrumentation scope exports");

  console.log("PASS otel");
} catch (e) {
  failed = true;
  console.error("FAIL otel:", e.message);
  if (stderr) console.error("--- server stderr ---\n" + stderr);
} finally {
  try {
    execSync(`pkill -P ${proc.pid}`);
  } catch {}
  try {
    proc.kill("SIGKILL");
  } catch {}
  try {
    execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
  } catch {}
  collector.close();
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
