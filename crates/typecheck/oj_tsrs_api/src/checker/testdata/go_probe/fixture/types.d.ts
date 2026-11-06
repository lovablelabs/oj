export interface Box<T> { value: T; [key: string]: unknown }
export declare function make<T>(v: T): Box<T>;
