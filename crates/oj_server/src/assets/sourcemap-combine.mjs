// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Rollup's `this.getCombinedSourcemap()` for a transform hook: the maps the
// earlier transforms of this module returned, collapsed into one map from
// the loaded code to the code the hook receives (rollup's collapseSourcemap,
// remapping each segment back through the chain). With no earlier map it is
// rollup's hires identity map of the loaded code; a transform that returned
// code without a map breaks the chain, and the map then maps nothing.

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));

// Each line's segments as [genCol, srcIdx, srcLine, srcCol, nameIdx?] (absolute).
export function decodeMappings(mappings) {
  const lines = [];
  let srcIdx = 0;
  let srcLine = 0;
  let srcCol = 0;
  let nameIdx = 0;
  for (const lineText of mappings.split(";")) {
    const line = [];
    let genCol = 0;
    for (const segText of lineText.split(",")) {
      if (segText === "") continue;
      const fields = [];
      let value = 0;
      let shift = 0;
      for (const ch of segText) {
        const digit = B64_INDEX.get(ch);
        if (digit === undefined) throw new Error(`invalid sourcemap mappings character ${JSON.stringify(ch)}`);
        value += (digit & 31) << shift;
        if (digit & 32) {
          shift += 5;
        } else {
          fields.push(value & 1 ? -(value >>> 1) : value >>> 1);
          value = 0;
          shift = 0;
        }
      }
      genCol += fields[0];
      if (fields.length === 1) {
        line.push([genCol]);
        continue;
      }
      srcIdx += fields[1];
      srcLine += fields[2];
      srcCol += fields[3];
      if (fields.length >= 5) {
        nameIdx += fields[4];
        line.push([genCol, srcIdx, srcLine, srcCol, nameIdx]);
      } else {
        line.push([genCol, srcIdx, srcLine, srcCol]);
      }
    }
    lines.push(line);
  }
  return lines;
}

function encodeValue(value) {
  let v = value < 0 ? (-value << 1) | 1 : value << 1;
  let out = "";
  do {
    let digit = v & 31;
    v >>>= 5;
    if (v > 0) digit |= 32;
    out += B64[digit];
  } while (v > 0);
  return out;
}

export function encodeMappings(lines) {
  let srcIdx = 0;
  let srcLine = 0;
  let srcCol = 0;
  let nameIdx = 0;
  const out = [];
  for (const line of lines) {
    let genCol = 0;
    const segs = [];
    for (const seg of line) {
      let s = encodeValue(seg[0] - genCol);
      genCol = seg[0];
      if (seg.length > 1) {
        s += encodeValue(seg[1] - srcIdx) + encodeValue(seg[2] - srcLine) + encodeValue(seg[3] - srcCol);
        srcIdx = seg[1];
        srcLine = seg[2];
        srcCol = seg[3];
        if (seg.length > 4) {
          s += encodeValue(seg[4] - nameIdx);
          nameIdx = seg[4];
        }
      }
      segs.push(s);
    }
    out.push(segs.join(","));
  }
  return out.join(";");
}

// The segment covering (line, column): the last at or before the column
// (trace-mapping's GREATEST_LOWER_BOUND), or null.
function segmentAt(lines, line, column) {
  const segs = lines[line];
  if (!segs || segs.length === 0) return null;
  let lo = 0;
  let hi = segs.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (segs[mid][0] <= column) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return found === -1 ? null : segs[found];
}

function parse(map) {
  const m = typeof map === "string" ? JSON.parse(map) : map;
  return {
    names: m.names ?? [],
    lines: decodeMappings(m.mappings ?? ""),
    sources: m.sources ?? [],
    sourcesContent: m.sourcesContent,
  };
}

// Rollup's hires identity map of `code` (MagicString's generateMap with
// `hires: true`): a segment at every character of every line.
function identityLines(code) {
  return code.split("\n").map((text, line) => {
    const segs = [];
    for (let col = 0; col < text.length; col++) segs.push([col, 0, line, col]);
    return segs;
  });
}

/**
 * The map from `originalCode` (the module `id` as loaded) to the output of
 * the last map in `chain` (oldest first). `broken`: a transform in the chain
 * returned code without a map.
 */
export function combineSourcemaps(id, originalCode, chain, broken = false) {
  const base = { version: 3, file: id, sources: [id], sourcesContent: [originalCode], names: [] };
  if (broken) return { ...base, mappings: "" };
  const maps = chain
    .filter((m) => m != null)
    .map(parse)
    .filter((m) => m.lines.some((l) => l.length > 0));
  if (maps.length === 0) return { ...base, mappings: encodeMappings(identityLines(originalCode)) };
  const names = [];
  const nameIndex = new Map();
  const nameOf = (name) => {
    if (!nameIndex.has(name)) {
      nameIndex.set(name, names.length);
      names.push(name);
    }
    return nameIndex.get(name);
  };
  const last = maps[maps.length - 1];
  const out = last.lines.map((segs) => {
    const line = [];
    for (const seg of segs) {
      // Each link of the chain has one source, the previous code (Rollup's
      // Link): a segment on any other source maps nowhere.
      if (seg.length === 1 || seg[1] !== 0) continue;
      let traced = seg;
      let name = seg.length > 4 ? last.names[seg[4]] : undefined;
      for (let i = maps.length - 2; i >= 0 && traced; i--) {
        const inner = segmentAt(maps[i].lines, traced[2], traced[3]);
        if (!inner || inner.length === 1 || inner[1] !== 0) {
          traced = null;
          break;
        }
        if (inner.length > 4) name = maps[i].names[inner[4]];
        traced = inner;
      }
      if (!traced) continue;
      line.push(
        name === undefined ? [seg[0], 0, traced[2], traced[3]] : [seg[0], 0, traced[2], traced[3], nameOf(name)],
      );
    }
    return line;
  });
  return { ...base, names, mappings: encodeMappings(out) };
}
