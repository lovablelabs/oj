import { type Box, make } from "./types";
export type Pair<A, B> = [first: A, second: B];
export type U = string | number;
export function over(x: string): string;
export function over(x: number): number;
export function over(x: any) { return x; }
const ünïcödé = "é😀";
export const box: Box<number> = make(1);
export const p: Pair<string, U> = ["a", 1];
export function isStr(x: unknown): x is string { return typeof x === "string"; }
/** Docs for Animal.
 * @deprecated use Dog */
export class Animal { name = "a"; }
export class Dog extends Animal { readonly legs = 4; }
export enum Color { Red = 1, Green = 2 }
export const r = over(42);
export type M = { [K in "a" | "b"]: K };
export type C<T> = T extends string ? 1 : 2;
export const lit = "hi" as const;
export const fn = <T,>(v: T) => v;
export const o = { box };
export { ünïcödé as uni };
export type IA<T, K extends keyof T> = T[K];
export async function aw(): Promise<number> { return 1; }
export let maybe: string | undefined;
export const doubled = [1, 2].map(x => x * 2);
export const cb: (n: number) => void = (n) => {};
export function withThis(this: Dog, a: number) { return a; }
export function rest(a: string, ...xs: number[]) { return xs; }
export const arr: number[] = [];
export type S<T> = T extends string ? Box<T> : never;
export interface WithDefault<T = string> { v: T }
export function useAll() { return box.value + p[1].toString(); }
export type Up = Uppercase<"a">;
export type TL = `x${string}`;
export const big = 10n;
