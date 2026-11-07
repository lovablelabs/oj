declare function empty(value: readonly []): void;
declare function nonempty(value: readonly [number]): void;
declare function array(value: readonly number[]): void;
empty([]);
nonempty([1]);
array([]);
