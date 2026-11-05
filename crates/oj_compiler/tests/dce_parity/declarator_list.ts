import { getRequest } from "@tanstack/react-start/server";
const a = getRequest,
  b = 2;
export const fn = keep(/*<*/() => a()/*>*/);
