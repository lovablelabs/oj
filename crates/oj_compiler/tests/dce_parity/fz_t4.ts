import { createServerOnlyFn } from "@tanstack/react-start";
export function make() { const a = g(); function k() { const a = side(); } return [k, createServerOnlyFn(/*<*/() => a/*>*/)]; }
