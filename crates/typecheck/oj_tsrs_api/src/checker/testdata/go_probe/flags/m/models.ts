export interface Box<T extends Base = Derived> {
    value: T;
    tuple: readonly [T, ...T[]];
    [key: string]: unknown;
    [index: number]: T;
    readonly opt?: true;
}
export class Base { base = true; }
export class Derived extends Base { derived = 1; }
export type Boxed = Box<Derived>;
export type TupleAlias = readonly [string, ...number[]];
export type ArrayAlias = Derived[];
export type EmptyTuple = [];
export type Union = Derived | string;
