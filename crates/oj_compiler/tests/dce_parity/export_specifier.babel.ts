import { s } from "./m";
const a = s;
const b = () => a;
export { b as renamed };
export const f = keep(undefined);
