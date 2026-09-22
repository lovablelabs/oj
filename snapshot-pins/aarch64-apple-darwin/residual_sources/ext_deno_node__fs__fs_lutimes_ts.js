// Copyright 2018-2026 the Deno authors. MIT license.
import { core, primordials } from "ext:core/mod.js";
import { op_node_lutimes, op_node_lutimes_sync } from "ext:core/ops";
const { promisify } = core.loadExtScript("ext:deno_node/internal/util.mjs");
import { getValidatedPathToString, toUnixTimestamp } from "ext:deno_node/internal/fs/utils.mjs";
const { Error, MathTrunc, Number, NumberIsFinite, NumberIsNaN, PromisePrototypeThen } = primordials;
function getValidUnixTime(value, name) {
  if (typeof value === "string") {
    value = Number(value);
  }
  if (typeof value === "number" && (NumberIsNaN(value) || !NumberIsFinite(value))) {
    throw new Deno.errors.InvalidData(`invalid ${name}, must not be infinity or NaN`);
  }
  const unixSeconds = toUnixTimestamp(value);
  const seconds = MathTrunc(unixSeconds);
  const nanoseconds = MathTrunc(unixSeconds * 1e3 - seconds * 1e3) * 1e6;
  return [
    seconds,
    nanoseconds
  ];
}
export function lutimes(path, atime, mtime, callback) {
  if (!callback) {
    throw new Error("No callback function supplied");
  }
  const { 0: atimeSecs, 1: atimeNanos } = getValidUnixTime(atime, "atime");
  const { 0: mtimeSecs, 1: mtimeNanos } = getValidUnixTime(mtime, "mtime");
  path = getValidatedPathToString(path);
  PromisePrototypeThen(op_node_lutimes(path, atimeSecs, atimeNanos, mtimeSecs, mtimeNanos), ()=>callback(null), callback);
}
export function lutimesSync(path, atime, mtime) {
  const { 0: atimeSecs, 1: atimeNanos } = getValidUnixTime(atime, "atime");
  const { 0: mtimeSecs, 1: mtimeNanos } = getValidUnixTime(mtime, "mtime");
  path = getValidatedPathToString(path);
  op_node_lutimes_sync(path, atimeSecs, atimeNanos, mtimeSecs, mtimeNanos);
}
export const lutimesPromise = promisify(lutimes);
